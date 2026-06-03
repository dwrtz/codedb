use std::path::Path;

use assert_cmd::Command;
use codedb::CodeDb;
use codedb::workspace::{WorkspaceRequest, WorkspaceResponse, execute_workspace_request};
use serde_json::{Value as JsonValue, json};
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn current_root(db: &Path) -> String {
    parse_json(&run(&["list", path(db), "--json"]))["root_hash"]
        .as_str()
        .expect("root hash")
        .to_string()
}

fn write_json(dir: &Path, name: &str, value: JsonValue) -> std::path::PathBuf {
    let file = dir.join(name);
    std::fs::write(&file, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    file
}

fn workspace_call(db: &mut CodeDb, method: &str, params: JsonValue) -> JsonValue {
    let response: WorkspaceResponse = execute_workspace_request(
        db,
        WorkspaceRequest {
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
fn symbol_and_expression_blame_follow_migration_history() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("provenance.sqlite");
    let rebuilt = temp.path().join("provenance-rebuilt.sqlite");
    let history = temp.path().join("history.ndjson");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    let tax_symbol = show_tax["symbol_hash"].as_str().unwrap().to_string();
    let original_tax_body = show_tax["body_hash"].as_str().unwrap().to_string();
    let blame_tax = parse_json(&run(&["blame-symbol", path(&db), "tax", "--json"]));
    assert_eq!(blame_tax["schema"], "codedb/blame-symbol/v1");
    assert_eq!(blame_tax["symbol_hash"], tax_symbol);
    assert_eq!(
        blame_tax["birth_migration"]["operation_kind"],
        "create_function"
    );
    assert_eq!(
        blame_tax["last_body_migration"]["operation_kind"],
        "create_function"
    );
    assert_eq!(
        blame_tax["last_signature_migration"]["operation_kind"],
        "create_function"
    );
    assert!(blame_tax["last_rename_migration"].is_null());

    let blame_original_body = parse_json(&run(&[
        "blame-expr",
        path(&db),
        &original_tax_body,
        "--json",
    ]));
    assert_eq!(blame_original_body["schema"], "codedb/blame-expr/v1");
    assert_eq!(blame_original_body["expr_hash"], original_tax_body);
    assert_eq!(blame_original_body["current_reachable"], true);
    assert_eq!(
        blame_original_body["introduced_migration"]["operation_kind"],
        "create_function"
    );
    assert!(
        blame_original_body["current_contexts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|context| context["name"] == "tax")
    );

    let root_after_import = current_root(&db);
    let rename_doc = write_json(
        temp.path(),
        "rename.json",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": root_after_import,
            "agent": {
                "agent_id": "agent:provenance",
                "request_id": "rename-tax"
            },
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                }
            ]
        }),
    );
    let rename = parse_json(&run(&["apply", path(&db), "--json", path(&rename_doc)]));
    assert_eq!(rename["status"], "applied");
    let rename_migration = rename["operations"][0]["migration_hash"]
        .as_str()
        .unwrap()
        .to_string();

    let body_doc = write_json(
        temp.path(),
        "body.json",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": rename["new_root_hash"],
            "agent": {
                "agent_id": "agent:provenance",
                "request_id": "replace-vat-body"
            },
            "operations": [
                {
                    "kind": "replace_function_body",
                    "name": "vat",
                    "body": {
                        "kind": "binary",
                        "op": "/",
                        "left": {
                            "kind": "binary",
                            "op": "*",
                            "left": {
                                "kind": "param_name",
                                "name": "subtotal"
                            },
                            "right": {
                                "kind": "literal_i64",
                                "value": "18"
                            }
                        },
                        "right": {
                            "kind": "literal_i64",
                            "value": "100"
                        }
                    }
                }
            ]
        }),
    );
    let body = parse_json(&run(&["apply", path(&db), "--json", path(&body_doc)]));
    assert_eq!(body["status"], "applied");
    let body_migration = body["operations"][0]["migration_hash"]
        .as_str()
        .unwrap()
        .to_string();

    let blame_vat = parse_json(&run(&["blame-symbol", path(&db), "vat", "--json"]));
    assert_eq!(blame_vat["symbol_hash"], tax_symbol);
    assert_eq!(
        blame_vat["birth_migration"]["migration_hash"],
        blame_tax["birth_migration"]["migration_hash"]
    );
    assert_eq!(
        blame_vat["last_rename_migration"]["migration_hash"],
        rename_migration
    );
    assert_eq!(
        blame_vat["last_name_migration"]["migration_hash"],
        rename_migration
    );
    assert_eq!(
        blame_vat["last_body_migration"]["migration_hash"],
        body_migration
    );
    assert_eq!(
        blame_vat["last_rename_migration"]["agent"]["request_id"],
        "rename-tax"
    );
    assert_eq!(
        blame_vat["last_body_migration"]["agent"]["agent_id"],
        "agent:provenance"
    );
    assert_eq!(
        blame_vat["last_body_migration"]["agent"]["request_id"],
        "replace-vat-body"
    );

    let show_vat = parse_json(&run(&["show", path(&db), "vat", "--json"]));
    let new_body = show_vat["body_hash"].as_str().unwrap().to_string();
    assert_ne!(new_body, original_tax_body);
    let blame_new_body = parse_json(&run(&["blame-expr", path(&db), &new_body, "--json"]));
    assert_eq!(blame_new_body["current_reachable"], true);
    assert_eq!(
        blame_new_body["introduced_migration"]["operation_kind"],
        "replace_function_body"
    );
    assert_eq!(
        blame_new_body["introduced_migration"]["migration_hash"],
        body_migration
    );
    assert_eq!(
        blame_new_body["introduced_migration"]["agent"]["request_id"],
        "replace-vat-body"
    );
    assert!(
        blame_new_body["current_contexts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|context| context["name"] == "vat")
    );

    let mut api_db = CodeDb::open(&db).unwrap();
    let api_symbol = workspace_call(
        &mut api_db,
        "provenance.blame_symbol",
        json!({"symbol_or_name": "vat"}),
    );
    assert_eq!(api_symbol["status"], "ok");
    assert_eq!(
        api_symbol["result"]["last_body_migration"]["migration_hash"],
        body_migration
    );
    let api_expr = workspace_call(
        &mut api_db,
        "provenance.blame_expr",
        json!({"expr_hash": new_body}),
    );
    assert_eq!(api_expr["status"], "ok");
    assert_eq!(
        api_expr["result"]["introduced_migration"]["migration_hash"],
        body_migration
    );

    run(&[
        "export-history",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&history),
    ]);
    run(&["init", path(&rebuilt)]);
    run(&["import-history", path(&rebuilt), path(&history)]);
    let rebuilt_blame = parse_json(&run(&["blame-symbol", path(&rebuilt), "vat", "--json"]));
    assert_eq!(
        rebuilt_blame["last_body_migration"]["migration_hash"],
        blame_vat["last_body_migration"]["migration_hash"]
    );
    assert_eq!(
        rebuilt_blame["last_body_migration"]["agent"],
        blame_vat["last_body_migration"]["agent"]
    );
    let rebuilt_expr = parse_json(&run(&["blame-expr", path(&rebuilt), &new_body, "--json"]));
    assert_eq!(
        rebuilt_expr["introduced_migration"]["migration_hash"],
        blame_new_body["introduced_migration"]["migration_hash"]
    );
    assert_eq!(
        rebuilt_expr["introduced_migration"]["agent"],
        blame_new_body["introduced_migration"]["agent"]
    );
}
