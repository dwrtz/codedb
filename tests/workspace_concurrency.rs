use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use codedb::CodeDb;
use codedb::workspace::{WorkspaceRequest, WorkspaceResponse, execute_workspace_request};
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
use tempfile::tempdir;

fn init_shop(db_path: &Path) -> CodeDb {
    let mut db = CodeDb::open(db_path).unwrap();
    db.init().unwrap();
    db.import_file(Path::new("examples/shop.cdb")).unwrap();
    db
}

fn workspace_call(db: &mut CodeDb, method: &str, params: JsonValue) -> WorkspaceResponse {
    execute_workspace_request(
        db,
        WorkspaceRequest {
            schema: None,
            jsonrpc: Some("2.0".to_string()),
            method: method.to_string(),
            params,
            id: None,
            request_id: None,
        },
    )
}

fn response_json(response: WorkspaceResponse) -> JsonValue {
    serde_json::to_value(response).unwrap()
}

fn branch_state(db_path: &Path) -> (String, Option<String>) {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT root_hash, history_hash FROM branches WHERE name = 'main'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn mutation_guard_counts(db_path: &Path) -> Vec<(String, i64)> {
    [
        "objects",
        "migrations",
        "histories",
        "branches",
        "root_symbols",
        "root_names",
        "root_exports",
        "dependencies",
        "workspace_transactions",
    ]
    .into_iter()
    .map(|table| (table.to_string(), row_count(db_path, table)))
    .collect()
}

fn row_count(db_path: &Path, table: &str) -> i64 {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn workspace_apply_rejects_stale_root_without_mutating_branch_or_history() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-stale.sqlite");
    let mut db = init_shop(&db_path);

    let agent_a = response_json(workspace_call(&mut db, "workspace.current", json!({})));
    let agent_b = response_json(workspace_call(&mut db, "workspace.current", json!({})));
    let inspected_root = agent_a["snapshot"]["root_hash"].as_str().unwrap();
    assert_eq!(agent_b["snapshot"]["root_hash"], inspected_root);

    let first_write = response_json(workspace_call(
        &mut db,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": inspected_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                }
            ]
        }),
    ));
    assert_eq!(first_write["status"], "ok");
    let actual_root = first_write["snapshot"]["root_hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(actual_root, inspected_root);

    let state_after_first_write = branch_state(&db_path);
    let counts_after_first_write = mutation_guard_counts(&db_path);

    let stale_write = response_json(workspace_call(
        &mut db,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": inspected_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "total",
                    "new_name": "sum"
                }
            ]
        }),
    ));
    assert_eq!(stale_write["status"], "error");
    assert_eq!(stale_write["error"]["kind"], "stale_root");
    assert_eq!(stale_write["error"]["expected_root_hash"], inspected_root);
    assert_eq!(stale_write["error"]["actual_root_hash"], actual_root);
    assert_eq!(stale_write["snapshot"]["root_hash"], actual_root);
    assert_eq!(branch_state(&db_path), state_after_first_write);
    assert_eq!(mutation_guard_counts(&db_path), counts_after_first_write);
}

#[test]
fn workspace_apply_requires_expected_root_and_rolls_back_batch_conflicts() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-batch-conflict.sqlite");
    let mut db = init_shop(&db_path);

    let current = response_json(workspace_call(&mut db, "workspace.current", json!({})));
    let root = current["snapshot"]["root_hash"].as_str().unwrap();
    let state_before = branch_state(&db_path);
    let counts_before = mutation_guard_counts(&db_path);

    let missing_root = response_json(workspace_call(
        &mut db,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                }
            ]
        }),
    ));
    assert_eq!(missing_root["status"], "error");
    assert_eq!(missing_root["error"]["kind"], "invalid_params");
    assert_eq!(branch_state(&db_path), state_before);
    assert_eq!(mutation_guard_counts(&db_path), counts_before);

    let conflict = response_json(workspace_call(
        &mut db,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                },
                {
                    "kind": "create_alias",
                    "name": "total",
                    "alias": "vat"
                }
            ]
        }),
    ));
    assert_eq!(conflict["status"], "error");
    assert_eq!(conflict["error"]["kind"], "name_conflict");
    assert_eq!(
        conflict["diagnostics"][0]["details"]["results"][0]["status"],
        "rolled_back"
    );
    assert_eq!(
        conflict["diagnostics"][0]["details"]["results"][1]["status"],
        "conflict"
    );
    assert_eq!(
        conflict["diagnostics"][0]["details"]["committed"],
        JsonValue::Bool(false)
    );
    assert_eq!(branch_state(&db_path), state_before);
    assert_eq!(mutation_guard_counts(&db_path), counts_before);

    let show_vat = response_json(workspace_call(
        &mut db,
        "symbols.show",
        json!({"name": "vat"}),
    ));
    assert_eq!(show_vat["status"], "error");
}

#[test]
fn workspace_apply_request_id_replays_committed_transaction_response() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-idempotency.sqlite");
    let mut db = init_shop(&db_path);

    let current = response_json(workspace_call(&mut db, "workspace.current", json!({})));
    let root = current["snapshot"]["root_hash"].as_str().unwrap();
    let transaction = json!({
        "schema": "codedb/workspace-transaction/v1",
        "branch": "main",
        "expected_root": root,
        "agent": {
            "agent_id": "agent:a",
            "request_id": "rename-tax-once"
        },
        "operations": [
            {
                "kind": "rename_symbol",
                "name": "tax",
                "new_name": "vat"
            }
        ]
    });

    let first = response_json(workspace_call(&mut db, "ops.apply", transaction.clone()));
    assert_eq!(first["status"], "ok");
    let root_after_first = first["snapshot"]["root_hash"].as_str().unwrap();
    assert_ne!(root_after_first, root);
    assert_eq!(row_count(&db_path, "workspace_transactions"), 1);
    let agent_json: String = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT agent_json FROM migrations
             WHERE operation_kind = 'rename_symbol'
             ORDER BY created_at DESC, hash DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let agent: JsonValue = serde_json::from_str(&agent_json).unwrap();
    assert_eq!(agent["agent_id"], "agent:a");
    assert_eq!(agent["request_id"], "rename-tax-once");

    let second = response_json(workspace_call(
        &mut db,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": root_after_first,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "total",
                    "new_name": "sum"
                }
            ]
        }),
    ));
    assert_eq!(second["status"], "ok");
    let root_after_second = second["snapshot"]["root_hash"].as_str().unwrap();
    assert_ne!(root_after_second, root_after_first);

    let retry = response_json(workspace_call(&mut db, "ops.apply", transaction.clone()));
    assert_eq!(retry["status"], "ok");
    assert_eq!(retry["result"], first["result"]);
    assert_eq!(retry["snapshot"], first["snapshot"]);

    let after_retry = response_json(workspace_call(&mut db, "workspace.current", json!({})));
    assert_eq!(after_retry["snapshot"]["root_hash"], root_after_second);

    let reused_token = response_json(workspace_call(
        &mut db,
        "ops.apply",
        json!({
            "schema": "codedb/workspace-transaction/v1",
            "branch": "main",
            "expected_root": root,
            "agent": {
                "agent_id": "agent:a",
                "request_id": "rename-tax-once"
            },
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "gst"
                }
            ]
        }),
    ));
    assert_eq!(reused_token["status"], "error");
    assert_eq!(reused_token["error"]["kind"], "invalid_request");
    assert_eq!(branch_state(&db_path).0, root_after_second);
}

#[test]
fn concurrent_duplicate_workspace_apply_request_id_replays_committed_response() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-concurrent-idempotency.sqlite");
    let mut db = init_shop(&db_path);
    let current = response_json(workspace_call(&mut db, "workspace.current", json!({})));
    let root = current["snapshot"]["root_hash"]
        .as_str()
        .unwrap()
        .to_string();
    drop(db);

    let transaction = Arc::new(json!({
        "schema": "codedb/workspace-transaction/v1",
        "branch": "main",
        "expected_root": root,
        "agent": {
            "agent_id": "agent:a",
            "request_id": "rename-tax-concurrent"
        },
        "operations": [
            {
                "kind": "rename_symbol",
                "name": "tax",
                "new_name": "vat"
            }
        ]
    }));
    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let db_path = db_path.clone();
            let transaction = Arc::clone(&transaction);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut db = CodeDb::open(db_path).unwrap();
                barrier.wait();
                response_json(workspace_call(&mut db, "ops.apply", (*transaction).clone()))
            })
        })
        .collect::<Vec<_>>();

    let responses = handles
        .into_iter()
        .map(|handle| handle.join().expect("workspace caller thread"))
        .collect::<Vec<_>>();

    assert_eq!(responses[0]["status"], "ok");
    assert_eq!(responses[1]["status"], "ok");
    assert_eq!(responses[0]["result"], responses[1]["result"]);
    assert_eq!(responses[0]["snapshot"], responses[1]["snapshot"]);
    assert_eq!(row_count(&db_path, "workspace_transactions"), 1);
}
