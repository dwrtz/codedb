use std::path::Path;

use assert_cmd::Command;
use serde_json::{Value as JsonValue, json};
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn workspace_call(db: &Path, method: &str, params: JsonValue) -> JsonValue {
    let mut codedb = codedb::CodeDb::open(db).unwrap();
    let response = codedb::workspace::execute_workspace_request(
        &mut codedb,
        codedb::workspace::WorkspaceRequest {
            schema: None,
            jsonrpc: Some("2.0".to_string()),
            method: method.to_string(),
            params,
            id: Some(json!(method)),
            request_id: None,
        },
    );
    serde_json::to_value(response).unwrap()
}

#[test]
fn effectful_functions_round_trip_and_are_queryable() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("effects.sqlite");
    let source = temp.path().join("effects.cdb");
    let projection = temp.path().join("effects.projection.cdb");
    let rebuilt = temp.path().join("effects-rebuilt.sqlite");

    std::fs::write(
        &source,
        r#"
fn read_counter() -> i64 effects[io] = 41
fn main() -> i64 effects[io] = read_counter() + 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "42");

    let listed = parse_json(&run(&["list", path(&db), "--json"]));
    let read_counter = listed["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "read_counter")
        .unwrap();
    assert_eq!(read_counter["effects"], json!(["io"]));
    assert!(
        read_counter["signature"]
            .as_str()
            .unwrap()
            .contains("effects[io]")
    );

    let show_main = parse_json(&run(&["show", path(&db), "main", "--json"]));
    assert_eq!(show_main["effects"], json!(["io"]));

    let impure = workspace_call(&db, "symbols.list", json!({ "effect": "io" }));
    assert_eq!(impure["status"], "ok");
    assert_eq!(impure["result"]["symbols"].as_array().unwrap().len(), 2);
    let pure = workspace_call(&db, "symbols.list", json!({ "effect": "pure" }));
    assert_eq!(pure["status"], "ok");
    assert!(pure["result"]["symbols"].as_array().unwrap().is_empty());

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("fn read_counter() -> i64 effects[io] = 41"));
    assert!(exported.contains("fn main() -> i64 effects[io] = read_counter() + 1"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "42");
    run(&["verify", path(&rebuilt)]);
}

#[test]
fn undeclared_callee_effect_rejects_apply_batch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-effects.sqlite");
    let apply = temp.path().join("bad-effects.apply.json");

    std::fs::write(
        &apply,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "operations": [
                {
                    "kind": "create_function",
                    "name": "read_counter",
                    "birth_seed": "effects-read-counter",
                    "params": [],
                    "return_type": "i64",
                    "effects": ["io"],
                    "body": { "kind": "literal_i64", "value": "41" }
                },
                {
                    "kind": "create_function",
                    "name": "main",
                    "birth_seed": "effects-main",
                    "params": [],
                    "return_type": "i64",
                    "body": {
                        "kind": "call",
                        "name": "read_counter",
                        "args": []
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    run(&["init", path(&db)]);
    let result = parse_json(&run(&["apply", path(&db), "--json", path(&apply)]));
    assert_eq!(result["status"], "error");
    assert_eq!(result["committed"], false);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("undeclared effect io")
    );
    let listed = parse_json(&run(&["list", path(&db), "--json"]));
    assert!(listed["symbols"].as_array().unwrap().is_empty());
}

#[test]
fn effect_only_signature_change_updates_interface_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("effect-change.sqlite");
    let source = temp.path().join("pure-main.cdb");

    std::fs::write(&source, "fn main() -> i64 = 1\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let before = parse_json(&run(&["show", path(&db), "main", "--json"]));
    assert_eq!(before["effects"], json!(["pure"]));

    let changed = parse_json(&run(&[
        "change-signature",
        path(&db),
        "main",
        "() -> i64 effects[io]",
        "--json",
    ]));
    assert_eq!(changed["status"], "applied");
    assert_eq!(
        changed["summary"]["build_impact"]["kind"],
        "recompile_dependents"
    );
    assert!(
        changed["summary"]["build_impact"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("interface_hash_changed"))
    );

    let after = parse_json(&run(&["show", path(&db), "main", "--json"]));
    assert_eq!(after["effects"], json!(["io"]));
    assert_ne!(before["signature_hash"], after["signature_hash"]);
    assert_eq!(before["body_hash"], after["body_hash"]);

    let build_plan = parse_json(&run(&["build-plan", path(&db), "main", "--json"]));
    assert_eq!(build_plan["entry_effects"], json!(["io"]));
    assert_eq!(build_plan["objects"][0]["effects"], json!(["io"]));

    run(&[
        "create-test",
        path(&db),
        "main_returns_1",
        "--entry",
        "main",
        "--expect-i64",
        "1",
    ]);
    let listed_tests = parse_json(&run(&["test", path(&db), "--list", "--json"]));
    assert_eq!(listed_tests["tests"][0]["entry_effects"], json!(["io"]));
    let run_tests = parse_json(&run(&["test", path(&db), "--json"]));
    assert_eq!(run_tests["tests"][0]["entry_effects"], json!(["io"]));
}
