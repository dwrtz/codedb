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
fn semantic_bisect_and_why_explain_behavior_change() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bisect.sqlite");
    let rebuilt = temp.path().join("bisect-rebuilt.sqlite");
    let history = temp.path().join("history.ndjson");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root_after_import = current_root(&db);
    let created_test = parse_json(&run(&[
        "create-test",
        path(&db),
        "main_returns_120",
        "--entry",
        "main",
        "--expect-i64",
        "120",
        "--expect-root",
        &root_after_import,
        "--json",
    ]));
    assert_eq!(created_test["status"], "applied");
    let root_before_change = created_test["new_root_hash"].as_str().unwrap().to_string();

    let changed = parse_json(&run(&[
        "replace-body",
        path(&db),
        "tax",
        "subtotal * 18 / 100",
        "--expect-root",
        &root_before_change,
        "--json",
    ]));
    assert_eq!(changed["status"], "applied");
    let change_migration = changed["migration_hash"].as_str().unwrap().to_string();
    let root_after_change = changed["new_root_hash"].as_str().unwrap().to_string();

    let bisect = parse_json(&run(&[
        "bisect-history",
        path(&db),
        "main",
        "--expect-output",
        "120",
        "--json",
    ]));
    assert_eq!(bisect["schema"], "codedb/bisect-history/v1");
    assert_eq!(bisect["status"], "changed");
    assert_eq!(
        bisect["first_changed"]["migration"]["migration_hash"],
        change_migration
    );
    assert_eq!(
        bisect["first_changed"]["previous_evaluation"]["actual"],
        json!({"kind": "i64", "value": "120"})
    );
    assert_eq!(
        bisect["first_changed"]["changed_evaluation"]["actual"],
        json!({"kind": "i64", "value": "118"})
    );

    let test_bisect = parse_json(&run(&[
        "bisect-history",
        path(&db),
        "--test",
        "main_returns_120",
        "--expect-test",
        "passed",
        "--json",
    ]));
    assert_eq!(test_bisect["status"], "changed");
    assert_eq!(
        test_bisect["first_changed"]["migration"]["migration_hash"],
        change_migration
    );
    assert_eq!(
        test_bisect["first_changed"]["changed_evaluation"]["actual_status"],
        "failed"
    );

    let failed_bisect = parse_json(&run(&[
        "bisect-history",
        path(&db),
        "--test",
        "main_returns_120",
        "--expect-test",
        "failed",
        "--json",
    ]));
    assert_eq!(failed_bisect["status"], "changed");
    assert_eq!(
        failed_bisect["first_changed"]["migration"]["migration_hash"],
        change_migration
    );
    assert_eq!(
        failed_bisect["first_changed"]["previous_evaluation"]["actual_status"],
        "passed"
    );
    assert_eq!(
        failed_bisect["first_changed"]["changed_evaluation"]["actual_status"],
        "failed"
    );

    let why = parse_json(&run(&[
        "why",
        path(&db),
        "main",
        "--from",
        &root_before_change,
        "--to",
        &root_after_change,
        "--json",
    ]));
    assert_eq!(why["schema"], "codedb/why/v1");
    assert_eq!(why["trace_summary"]["result_changed"], true);
    assert_eq!(why["direct_migration"]["migration_hash"], change_migration);
    assert_eq!(why["changed_functions"][0]["function"], "tax");
    let expression_changes = why["changed_functions"][0]["expression_changes"]
        .as_array()
        .expect("expression changes");
    assert!(expression_changes.iter().any(|change| {
        change["kind"] == "literal_changed"
            && change["from_value"] == "20"
            && change["to_value"] == "18"
    }));

    let mut api_db = CodeDb::open(&db).unwrap();
    let api_bisect = workspace_call(
        &mut api_db,
        "history.bisect",
        json!({"entry_name": "main", "expect_output": "120"}),
    );
    assert_eq!(api_bisect["status"], "ok");
    assert_eq!(
        api_bisect["result"]["first_changed"]["migration"]["migration_hash"],
        change_migration
    );
    let api_why = workspace_call(
        &mut api_db,
        "why.run",
        json!({
            "entry_name": "main",
            "from": root_before_change,
            "to": root_after_change,
        }),
    );
    assert_eq!(api_why["status"], "ok");
    assert_eq!(
        api_why["result"]["direct_migration"]["migration_hash"],
        change_migration
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
    let rebuilt_bisect = parse_json(&run(&[
        "bisect-history",
        path(&rebuilt),
        "main",
        "--expect-output",
        "120",
        "--json",
    ]));
    assert_eq!(
        rebuilt_bisect["first_changed"]["migration"]["migration_hash"],
        change_migration
    );
}

#[test]
fn semantic_bisect_reports_first_change_when_behavior_is_restored() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bisect-restored.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root_before_change = current_root(&db);
    let changed = parse_json(&run(&[
        "replace-body",
        path(&db),
        "tax",
        "subtotal * 18 / 100",
        "--expect-root",
        &root_before_change,
        "--json",
    ]));
    assert_eq!(changed["status"], "applied");
    let change_migration = changed["migration_hash"].as_str().unwrap().to_string();
    let root_after_change = changed["new_root_hash"].as_str().unwrap().to_string();

    let restored = parse_json(&run(&[
        "replace-body",
        path(&db),
        "tax",
        "subtotal * 20 / 100",
        "--expect-root",
        &root_after_change,
        "--json",
    ]));
    assert_eq!(restored["status"], "applied");
    let root_after_restore = restored["new_root_hash"].as_str().unwrap().to_string();
    assert_eq!(root_after_restore, root_before_change);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");

    let bisect = parse_json(&run(&[
        "bisect-history",
        path(&db),
        "main",
        "--expect-output",
        "120",
        "--json",
    ]));
    assert_eq!(bisect["status"], "changed");
    assert_eq!(
        bisect["first_changed"]["migration"]["migration_hash"],
        change_migration
    );
    assert_eq!(
        bisect["first_changed"]["previous_evaluation"]["actual"],
        json!({"kind": "i64", "value": "120"})
    );
    assert_eq!(
        bisect["first_changed"]["changed_evaluation"]["actual"],
        json!({"kind": "i64", "value": "118"})
    );

    let changed_again = parse_json(&run(&[
        "replace-body",
        path(&db),
        "tax",
        "subtotal * 19 / 100",
        "--expect-root",
        &root_after_restore,
        "--json",
    ]));
    assert_eq!(changed_again["status"], "applied");
    let change_again_migration = changed_again["migration_hash"]
        .as_str()
        .unwrap()
        .to_string();
    let root_after_second_change = changed_again["new_root_hash"].as_str().unwrap().to_string();

    let why = parse_json(&run(&[
        "why",
        path(&db),
        "main",
        "--from",
        &root_after_restore,
        "--to",
        &root_after_second_change,
        "--json",
    ]));
    assert_eq!(why["summary"]["migration_count"], 1);
    assert_eq!(
        why["direct_migration"]["migration_hash"],
        change_again_migration
    );
    assert_eq!(
        why["migration_path"]
            .as_array()
            .expect("migration path")
            .len(),
        1
    );
}

#[test]
fn semantic_bisect_supports_test_error_predicate() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bisect-test-error.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root_after_import = current_root(&db);
    let created_test = parse_json(&run(&[
        "create-test",
        path(&db),
        "main_returns_120",
        "--entry",
        "main",
        "--expect-i64",
        "120",
        "--expect-root",
        &root_after_import,
        "--json",
    ]));
    assert_eq!(created_test["status"], "applied");
    let create_test_migration = created_test["migration_hash"].as_str().unwrap().to_string();

    let bisect = parse_json(&run(&[
        "bisect-history",
        path(&db),
        "--test",
        "main_returns_120",
        "--expect-test",
        "error",
        "--json",
    ]));
    assert_eq!(bisect["status"], "changed");
    assert_eq!(
        bisect["first_changed"]["migration"]["migration_hash"],
        create_test_migration
    );
    assert_eq!(
        bisect["first_changed"]["previous_evaluation"]["actual_status"],
        "error"
    );
    assert_eq!(
        bisect["first_changed"]["changed_evaluation"]["actual_status"],
        "passed"
    );
}
