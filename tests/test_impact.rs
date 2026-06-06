use assert_cmd::Command;
use serde_json::{Value as JsonValue, json};
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn path(path: &std::path::Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn current_root(db: &std::path::Path) -> String {
    parse_json(&run(&["list", path(db), "--json"]))["root_hash"]
        .as_str()
        .expect("root hash")
        .to_string()
}

fn test_status<'a>(report: &'a JsonValue, name: &str) -> &'a JsonValue {
    report["tests"]
        .as_array()
        .expect("tests array")
        .iter()
        .find(|test| test["name"] == name)
        .unwrap_or_else(|| panic!("missing test {name}: {report}"))
}

fn reason_kinds(test: &JsonValue) -> Vec<&str> {
    test["reasons"]
        .as_array()
        .expect("reasons array")
        .iter()
        .filter_map(|reason| reason["kind"].as_str())
        .collect()
}

fn workspace_call(db: &std::path::Path, method: &str, params: JsonValue) -> JsonValue {
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
fn test_impact_selects_reachable_body_changes_and_skips_unrelated_changes() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("shop-with-unused.cdb");
    std::fs::write(
        &source,
        "fn tax(subtotal: i64) -> i64 = subtotal * 20 / 100\n\
         \n\
         fn total(subtotal: i64) -> i64 = subtotal + tax(subtotal)\n\
         \n\
         fn main() -> i64 = total(100)\n\
         \n\
         fn unused() -> i64 = 1\n",
    )
    .unwrap();

    let db = temp.path().join("reachable.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "create-test",
        path(&db),
        "main_returns_120",
        "--entry",
        "main",
        "--expect-i64",
        "120",
    ]);
    let old_root = current_root(&db);
    run(&["replace-body", path(&db), "tax", "subtotal * 18 / 100"]);
    let new_root = current_root(&db);
    let report = parse_json(&run(&[
        "test-impact",
        path(&db),
        &old_root,
        &new_root,
        "--json",
    ]));
    assert_eq!(report["schema"], "codedb/test-impact/v1");
    assert_eq!(report["selected"], 1);
    assert_eq!(report["skipped"], 0);
    let main_test = test_status(&report, "main_returns_120");
    assert_eq!(main_test["status"], "selected");
    assert!(reason_kinds(main_test).contains(&"changed_symbol_reachable"));
    assert!(
        report["changed_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "tax"
                && symbol["categories"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("behavior")))
    );

    let unrelated_db = temp.path().join("unrelated.sqlite");
    run(&["init", path(&unrelated_db)]);
    run(&["import", path(&unrelated_db), path(&source)]);
    run(&[
        "create-test",
        path(&unrelated_db),
        "main_returns_120",
        "--entry",
        "main",
        "--expect-i64",
        "120",
    ]);
    let old_root = current_root(&unrelated_db);
    run(&["replace-body", path(&unrelated_db), "unused", "2"]);
    let new_root = current_root(&unrelated_db);
    let report = parse_json(&run(&[
        "test-impact",
        path(&unrelated_db),
        &old_root,
        &new_root,
        "--json",
    ]));
    assert_eq!(report["selected"], 0);
    assert_eq!(report["skipped"], 1);
    let main_test = test_status(&report, "main_returns_120");
    assert_eq!(main_test["status"], "skipped");
    assert!(reason_kinds(main_test).contains(&"unaffected_dependency_closure"));
}

#[test]
fn test_impact_selects_behavior_tests_for_type_definition_changes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("type-change.sqlite");
    let source = temp.path().join("type-change.cdb");
    let add_field = temp.path().join("add-field.json");

    std::fs::write(
        &source,
        "record Config {\n\
           flag: i64\n\
         }\n\
         \n\
         fn main() -> i64 = 1\n\
         \n\
         fn unused() -> i64 = 1\n",
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "create-test",
        path(&db),
        "main_returns_1",
        "--entry",
        "main",
        "--expect-i64",
        "1",
    ]);
    let old_root = current_root(&db);
    std::fs::write(
        &add_field,
        r#"{ "kind": "add_field", "type": "Config", "field": { "name": "limit", "type": "i64" } }"#,
    )
    .unwrap();
    run(&["apply", path(&db), "--json", path(&add_field)]);
    run(&["replace-body", path(&db), "unused", "2"]);
    let new_root = current_root(&db);

    let report = parse_json(&run(&[
        "test-impact",
        path(&db),
        &old_root,
        &new_root,
        "--json",
    ]));
    assert_eq!(report["selected"], 1);
    assert_eq!(report["skipped"], 0);
    let test = test_status(&report, "main_returns_1");
    assert_eq!(test["status"], "selected");
    assert!(reason_kinds(test).contains(&"type_definition_changed"));
}

#[test]
fn test_impact_skips_behavior_rename_but_selects_projection_category() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("rename.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);
    run(&[
        "create-test",
        path(&db),
        "main_behavior",
        "--entry",
        "main",
        "--expect-i64",
        "120",
    ]);
    run(&[
        "create-test",
        path(&db),
        "main_projection",
        "--entry",
        "main",
        "--expect-i64",
        "120",
        "--category",
        "projection",
    ]);
    let old_root = current_root(&db);
    run(&["rename", path(&db), "tax", "vat"]);
    let new_root = current_root(&db);
    let report = parse_json(&run(&[
        "test-impact",
        path(&db),
        &old_root,
        &new_root,
        "--json",
    ]));

    assert_eq!(report["selected"], 1);
    assert_eq!(report["skipped"], 1);
    let behavior = test_status(&report, "main_behavior");
    assert_eq!(behavior["category"], "behavior");
    assert_eq!(behavior["status"], "skipped");
    assert!(reason_kinds(behavior).contains(&"metadata_only_change"));

    let projection = test_status(&report, "main_projection");
    assert_eq!(projection["category"], "projection");
    assert_eq!(projection["status"], "selected");
    assert!(reason_kinds(projection).contains(&"projection_symbol_reachable"));
}

#[test]
fn test_impact_selects_signature_changes_and_workspace_api_exposes_report() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("signature.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);
    run(&[
        "create-test",
        path(&db),
        "main_returns_120",
        "--entry",
        "main",
        "--expect-i64",
        "120",
    ]);
    let old_root = current_root(&db);
    run(&["replace-body", path(&db), "total", "subtotal"]);
    run(&[
        "change-signature",
        path(&db),
        "tax",
        "(subtotal: i64, rate: i64) -> i64",
    ]);
    run(&["replace-body", path(&db), "tax", "subtotal * rate / 100"]);
    run(&[
        "replace-body",
        path(&db),
        "total",
        "subtotal + tax(subtotal, 20)",
    ]);
    let new_root = current_root(&db);

    let report = parse_json(&run(&[
        "test-impact",
        path(&db),
        &old_root,
        &new_root,
        "--json",
    ]));
    assert_eq!(report["selected"], 1);
    let test = test_status(&report, "main_returns_120");
    assert_eq!(test["status"], "selected");
    assert!(reason_kinds(test).contains(&"changed_symbol_reachable"));
    assert!(
        report["changed_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "tax"
                && symbol["reasons"]["behavior"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("signature_changed")))
    );

    let response = workspace_call(
        &db,
        "tests.impact",
        json!({
            "old_root_hash": old_root,
            "new_root_hash": new_root
        }),
    );
    assert_eq!(response["status"], "ok");
    assert_eq!(response["result"]["schema"], "codedb/test-impact/v1");
    assert_eq!(response["result"]["selected"], 1);
    assert_eq!(response["snapshot"]["branch"], "main");
}
