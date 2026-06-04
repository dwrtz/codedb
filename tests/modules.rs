use std::path::Path;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json {err}: {text}"))
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn current_root(db: &Path) -> String {
    parse_json(&run(&["list", path(db), "--json"]))["root_hash"]
        .as_str()
        .unwrap()
        .to_string()
}

fn object_cache_count(db: &Path) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM compile_cache WHERE artifact_kind = 'object_file'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn moving_symbol_between_modules_preserves_identity_and_round_trips_projection() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("modules.sqlite");
    let before_obj = temp.path().join("tax-before.o");
    let after_obj = temp.path().join("tax-after.o");
    let projection = temp.path().join("projection.cdb");
    let rebuilt = temp.path().join("rebuilt.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let tax_before = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    let tax_symbol = tax_before["symbol_hash"].as_str().unwrap().to_string();
    let tax_definition = tax_before["definition_hash"].as_str().unwrap().to_string();
    run(&[
        "emit-object",
        path(&db),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&before_obj),
    ]);
    assert_eq!(object_cache_count(&db), 1);
    let before_bytes = std::fs::read(&before_obj).unwrap();

    let root_before = current_root(&db);
    let moved = parse_json(&run(&[
        "module",
        "move-symbol",
        path(&db),
        "tax",
        "billing",
        "--expect-root",
        &root_before,
        "--json",
    ]));
    assert_eq!(moved["status"], "applied");
    assert_eq!(moved["summary"]["kind"], "move_symbol");
    assert_eq!(moved["summary"]["semantic_impact"], "symbol_moved");
    assert_eq!(moved["summary"]["build_impact"]["kind"], "metadata_only");
    assert_eq!(moved["summary"]["build_impact"]["relink"], false);
    assert!(
        moved["summary"]["build_impact"]["recompile"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let tax_after = parse_json(&run(&["show", path(&db), "billing.tax", "--json"]));
    assert_eq!(tax_after["symbol_hash"], tax_symbol);
    assert_eq!(tax_after["definition_hash"], tax_definition);
    assert_eq!(tax_after["module"], "billing");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");

    run(&[
        "emit-object",
        path(&db),
        "billing.tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&after_obj),
    ]);
    assert_eq!(object_cache_count(&db), 1);
    assert_eq!(std::fs::read(&after_obj).unwrap(), before_bytes);

    let modules = parse_json(&run(&["module", "list", path(&db), "--json"]));
    let module_names = modules["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|module| module["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(module_names, vec!["billing", "main"]);
    let billing = parse_json(&run(&["module", "show", path(&db), "billing", "--json"]));
    assert_eq!(billing["symbols"][0]["name"], "tax");
    assert_eq!(billing["symbols"][0]["symbol_hash"], tax_symbol);

    let diff = parse_json(&run(&[
        "diff",
        path(&db),
        &root_before,
        moved["new_root_hash"].as_str().unwrap(),
        "--json",
    ]));
    assert_eq!(diff["build_impact"]["kind"], "metadata_only");
    assert_eq!(diff["changes"][0]["kind"], "symbol_moved");
    assert_eq!(diff["changes"][0]["from"], "main.tax");
    assert_eq!(diff["changes"][0]["to"], "billing.tax");

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let source = std::fs::read_to_string(&projection).unwrap();
    assert!(source.contains("module billing {\nfn tax(subtotal: i64) -> i64"));
    assert!(source.contains("subtotal + billing.tax(subtotal)"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "120");
    let rebuilt_tax = parse_json(&run(&["show", path(&rebuilt), "billing.tax", "--json"]));
    assert_eq!(rebuilt_tax["module"], "billing");
}

#[test]
fn module_name_conflicts_are_scoped_to_the_target_module() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("module-conflicts.sqlite");
    let source = temp.path().join("source.cdb");
    std::fs::write(
        &source,
        r#"
fn tax(x: i64) -> i64 = x
fn main() -> i64 = tax(1)
module billing {
fn tax(x: i64) -> i64 = x + 1
}
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    let modules = parse_json(&run(&["module", "list", path(&db), "--json"]));
    assert_eq!(modules["modules"][0]["name"], "billing");
    assert_eq!(modules["modules"][1]["name"], "main");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "1");

    let root = current_root(&db);
    let conflict = parse_json(&run(&[
        "module",
        "move-symbol",
        path(&db),
        "tax",
        "billing",
        "--expect-root",
        &root,
        "--json",
    ]));
    assert_eq!(conflict["status"], "conflict");
    assert_eq!(conflict["failed_preconditions"][0], "name_is_available");
}

#[test]
fn workspace_module_methods_list_show_and_move_symbols() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-modules.sqlite");
    run(&["init", path(&db_path)]);
    run(&["import", path(&db_path), "examples/shop.cdb"]);

    let root = current_root(&db_path);
    let mut db = codedb::CodeDb::open(&db_path).unwrap();
    let moved = codedb::workspace::execute_workspace_request(
        &mut db,
        codedb::workspace::WorkspaceRequest {
            schema: Some(codedb::workspace::WORKSPACE_REQUEST_SCHEMA.to_string()),
            jsonrpc: None,
            method: "modules.move_symbol".to_string(),
            params: json!({
                "symbol_or_name": "tax",
                "module": "billing",
                "expected_root": root,
            }),
            id: None,
            request_id: None,
        },
    );
    assert_eq!(moved.status, "ok");
    let moved_result = moved.result.unwrap();
    assert_eq!(moved_result["status"], "applied");
    assert_eq!(
        moved_result["operations"][0]["summary"]["kind"],
        "move_symbol"
    );
    assert_eq!(moved.snapshot.unwrap().branch, "main");

    let listed = codedb::workspace::execute_workspace_request(
        &mut db,
        codedb::workspace::WorkspaceRequest {
            schema: None,
            jsonrpc: None,
            method: "modules.list".to_string(),
            params: json!({}),
            id: None,
            request_id: None,
        },
    );
    assert_eq!(listed.status, "ok");
    let listed_result = listed.result.unwrap();
    let modules = listed_result["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|module| module["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(modules, vec!["billing", "main"]);

    let shown = codedb::workspace::execute_workspace_request(
        &mut db,
        codedb::workspace::WorkspaceRequest {
            schema: None,
            jsonrpc: None,
            method: "modules.show".to_string(),
            params: json!({ "module": "billing" }),
            id: None,
            request_id: None,
        },
    );
    assert_eq!(shown.status, "ok");
    let result = shown.result.unwrap();
    assert_eq!(result["module"], "billing");
    assert_eq!(result["symbols"][0]["name"], "tax");
}
