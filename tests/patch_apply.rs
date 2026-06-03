use std::path::Path;

use assert_cmd::Command;
use codedb::CodeDb;
use codedb::workspace::{WorkspaceRequest, WorkspaceResponse, execute_workspace_request};
use rusqlite::Connection;
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

fn branch_state(db: &Path) -> (String, Option<String>) {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT root_hash, history_hash FROM branches WHERE name = 'main'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn row_count(db: &Path, table: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn mutation_counts(db: &Path) -> Vec<(String, i64)> {
    [
        "objects",
        "object_edges",
        "migrations",
        "histories",
        "branches",
        "root_symbols",
        "root_names",
        "root_exports",
        "dependencies",
        "compile_cache",
        "artifact_jobs",
        "workspace_transactions",
        "source_search",
    ]
    .into_iter()
    .map(|table| (table.to_string(), row_count(db, table)))
    .collect()
}

fn write_patch(dir: &Path, name: &str, patch: JsonValue) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(&patch).unwrap()).unwrap();
    path
}

fn enter_event_for_symbol<'a>(trace: &'a JsonValue, symbol_hash: &str) -> &'a JsonValue {
    trace["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|event| {
            event["event"] == "enter_function" && event["symbol_hash"].as_str() == Some(symbol_hash)
        })
        .unwrap_or_else(|| panic!("missing enter_function event for {symbol_hash}"))
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
fn semantic_patch_apply_replaces_literal_and_records_provenance() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("literal-apply.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root = current_root(&db);
    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    let tax_symbol = show_tax["symbol_hash"].as_str().unwrap();
    let before_trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let before_tax_enter = enter_event_for_symbol(&before_trace, tax_symbol);
    assert_eq!(
        before_trace["result"],
        json!({"kind": "i64", "value": "120"})
    );
    let patch = write_patch(
        temp.path(),
        "literal.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "agent": {
                "agent_id": "agent:patch-test",
                "request_id": "tax-rate-18"
            },
            "match": {
                "kind": "literal_i64",
                "value": "20",
                "within_name": "tax"
            },
            "replace": {
                "kind": "literal_i64",
                "value": "18"
            }
        }),
    );

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["schema"], "codedb/semantic-patch-apply-result/v1");
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["committed"], true);
    assert_eq!(applied["old_root_hash"], root);
    assert_ne!(applied["new_root_hash"], root);
    assert_eq!(applied["match_count"], 1);
    assert_eq!(applied["planned_operation_count"], 1);
    assert_eq!(
        applied["planned_operations"][0]["kind"],
        "replace_function_body"
    );
    assert_eq!(applied["semantic_summary"]["matched_expression_count"], 1);
    assert_eq!(
        applied["semantic_summary"]["operation_kinds"],
        json!(["replace_function_body"])
    );
    assert_eq!(applied["build_impact"]["kind"], "recompile_symbols");
    assert_eq!(applied["apply_result"]["committed"], true);

    let show_tax_after = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    assert_eq!(show_tax_after["body_source"], "subtotal * 18 / 100");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "118");
    let after_trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let after_tax_enter = enter_event_for_symbol(&after_trace, tax_symbol);
    assert_eq!(
        after_trace["result"],
        json!({"kind": "i64", "value": "118"})
    );
    assert_ne!(
        before_tax_enter["function_def_hash"],
        after_tax_enter["function_def_hash"]
    );
    assert!(
        after_trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| {
                event["event"] == "value" && event["value"] == json!({"kind": "i64", "value": "18"})
            })
    );

    let agent_json: String = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT agent_json FROM migrations
             WHERE operation_kind = 'replace_function_body'
             ORDER BY created_at DESC, hash DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let agent: JsonValue = serde_json::from_str(&agent_json).unwrap();
    assert_eq!(agent["agent_id"], "agent:patch-test");
    assert_eq!(agent["request_id"], "tax-rate-18");
    assert_eq!(
        agent["semantic_patch"]["schema"],
        "codedb/semantic-patch-provenance/v1"
    );
    assert_eq!(agent["semantic_patch"]["patch_hash"], applied["patch_hash"]);
    assert_eq!(
        agent["semantic_patch"]["planned_operation_kinds"],
        json!(["replace_function_body"])
    );
}

#[test]
fn semantic_patch_apply_retries_expected_root_without_duplicate_history() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("literal-retry.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root = current_root(&db);
    let patch = write_patch(
        temp.path(),
        "retry.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "literal_i64",
                "value": "20",
                "within_name": "tax"
            },
            "replace": {
                "kind": "literal_i64",
                "value": "18"
            }
        }),
    );

    let first = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(first["status"], "applied");
    let migration_count = row_count(&db, "migrations");
    let history_count = row_count(&db, "histories");

    let retry = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(retry["status"], "already_applied");
    assert_eq!(retry["committed"], false);
    assert_eq!(retry["new_root_hash"], first["new_root_hash"]);
    assert_eq!(row_count(&db, "migrations"), migration_count);
    assert_eq!(row_count(&db, "histories"), history_count);
}

#[test]
fn semantic_patch_apply_type_error_rolls_back_everything() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("literal-type-error.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root = current_root(&db);
    let branch_before = branch_state(&db);
    let counts_before = mutation_counts(&db);
    let patch = write_patch(
        temp.path(),
        "type-error.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "literal_i64",
                "value": "20",
                "within_name": "tax"
            },
            "replace": {
                "kind": "literal_bool",
                "value": true
            }
        }),
    );

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "error");
    assert_eq!(applied["committed"], false);
    assert_eq!(applied["typecheck"]["status"], "error");
    assert_eq!(applied["apply_result"]["rollback_reason"], "error");
    assert_eq!(branch_state(&db), branch_before);
    assert_eq!(mutation_counts(&db), counts_before);
    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    assert_eq!(show_tax["body_source"], "subtotal * 20 / 100");
}

#[test]
fn workspace_patch_apply_commits_and_reports_stale_root_conflict() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-patch-apply.sqlite");
    let mut db = CodeDb::open(&db_path).unwrap();
    db.init().unwrap();
    db.import_file(Path::new("examples/shop.cdb")).unwrap();

    let before = branch_state(&db_path);
    let applied = workspace_call(
        &mut db,
        "patch.apply",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": before.0,
            "match": {
                "kind": "literal_i64",
                "value": "20",
                "within_name": "tax"
            },
            "replace": {
                "kind": "literal_i64",
                "value": "18"
            }
        }),
    );
    assert_eq!(applied["schema"], "codedb/response/v1");
    assert_eq!(applied["status"], "ok");
    assert_eq!(
        applied["result"]["schema"],
        "codedb/semantic-patch-apply-result/v1"
    );
    assert_eq!(applied["result"]["status"], "applied");
    assert_eq!(applied["result"]["committed"], true);
    assert_eq!(
        applied["snapshot"]["root_hash"],
        applied["result"]["new_root_hash"]
    );

    let after_patch = branch_state(&db_path);
    let stale = workspace_call(
        &mut db,
        "patch.apply",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": before.0,
            "match": {
                "kind": "symbol",
                "name": "tax"
            },
            "replace": {
                "kind": "rename_symbol",
                "new_name": "vat"
            }
        }),
    );
    assert_eq!(stale["status"], "error");
    assert_eq!(stale["error"]["kind"], "stale_root");
    assert_eq!(stale["error"]["expected_root_hash"], before.0);
    assert_eq!(stale["error"]["actual_root_hash"], after_patch.0);
    assert_eq!(branch_state(&db_path), after_patch);
}
