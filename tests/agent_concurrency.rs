//! Multi-agent optimistic-concurrency hardening (PLAN_V3 Phase 3).
//!
//! Several agents build the compiler concurrently against the `--expect-root`
//! protocol. These tests prove the serialization guarantees N writers rely on:
//! racing the same root, exactly one applies and the branch advances by exactly
//! one root (no lost updates); and N identical (same request_id) submissions
//! replay one committed response (idempotency), recording one transaction.
//!
//! The underlying guarantees are: `BEGIN IMMEDIATE` serializes writers,
//! `busy_timeout` makes them wait rather than fail, the `RootIsCurrent`
//! precondition is re-checked under the write lock, and `workspace_transactions`
//! has a `request_id` primary key with `ON CONFLICT DO NOTHING`.

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use codedb::CodeDb;
use codedb::workspace::{WorkspaceRequest, WorkspaceResponse, execute_workspace_request};
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
use tempfile::tempdir;

const WRITERS: usize = 8;

fn init_shop(db_path: &Path) -> CodeDb {
    let mut db = CodeDb::open(db_path).unwrap();
    db.init().unwrap();
    db.import_file(Path::new("examples/shop.cdb")).unwrap();
    db
}

fn workspace_call(db: &mut CodeDb, method: &str, params: JsonValue) -> JsonValue {
    let response: WorkspaceResponse = execute_workspace_request(
        db,
        WorkspaceRequest {
            schema: None,
            jsonrpc: Some("2.0".to_string()),
            method: method.to_string(),
            params,
            id: None,
            request_id: None,
        },
    );
    serde_json::to_value(response).unwrap()
}

fn row_count(db_path: &Path, table: &str) -> i64 {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn branch_root(db_path: &Path) -> String {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT root_hash FROM branches WHERE name = 'main'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn committed(response: &JsonValue) -> bool {
    response["status"] == "ok" && response["result"]["committed"] == true
}

#[test]
fn n_writers_distinct_request_ids_serialize_to_one_applied() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("agent-race.sqlite");
    let mut db = init_shop(&db_path);
    let root = workspace_call(&mut db, "workspace.current", json!({}))["snapshot"]["root_hash"]
        .as_str()
        .unwrap()
        .to_string();
    drop(db);
    let migrations_before = row_count(&db_path, "migrations");

    // N agents each inspected the same root and each attempts a different,
    // non-idempotent write (distinct request_id + distinct rename) against it.
    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles = (0..WRITERS)
        .map(|i| {
            let db_path = db_path.clone();
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut db = CodeDb::open(&db_path).unwrap();
                let transaction = json!({
                    "schema": "codedb/workspace-transaction/v1",
                    "branch": "main",
                    "expected_root": root,
                    "agent": { "agent_id": format!("agent:{i}"), "request_id": format!("writer-{i}") },
                    "operations": [
                        { "kind": "rename_symbol", "name": "tax", "new_name": format!("vat{i}") }
                    ]
                });
                barrier.wait();
                workspace_call(&mut db, "ops.apply", transaction)
            })
        })
        .collect::<Vec<_>>();

    let responses = handles
        .into_iter()
        .map(|handle| handle.join().expect("writer thread"))
        .collect::<Vec<_>>();

    // Exactly one writer commits.
    let winners = responses.iter().filter(|r| committed(r)).count();
    assert_eq!(winners, 1, "exactly one writer must apply: {responses:#?}");

    // Every other writer is a recognized non-commit: a stale-root error (it read
    // the moved root) or a conflict (it lost the race under the write lock).
    for response in responses.iter().filter(|r| !committed(r)) {
        let stale = response["status"] == "error"
            && response["error"]["kind"] == "stale_root";
        let conflict = response["result"]["status"] == "conflict"
            || response["result"]["committed"] == false;
        assert!(
            stale || conflict,
            "loser must be stale_root or conflict, got: {response:#?}"
        );
    }

    // The branch advanced by exactly one root / one migration — no lost updates,
    // no torn state.
    let winner = responses.iter().find(|r| committed(r)).unwrap();
    assert_eq!(branch_root(&db_path), winner["result"]["new_root_hash"]);
    assert_ne!(branch_root(&db_path), root);
    assert_eq!(row_count(&db_path, "migrations"), migrations_before + 1);
}

#[test]
fn n_writers_same_request_id_replay_one_committed_response() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("agent-idempotent.sqlite");
    let mut db = init_shop(&db_path);
    let root = workspace_call(&mut db, "workspace.current", json!({}))["snapshot"]["root_hash"]
        .as_str()
        .unwrap()
        .to_string();
    drop(db);
    let migrations_before = row_count(&db_path, "migrations");

    // N agents submit the identical transaction (same request_id): one commits,
    // the rest replay its cached response.
    let transaction = Arc::new(json!({
        "schema": "codedb/workspace-transaction/v1",
        "branch": "main",
        "expected_root": root,
        "agent": { "agent_id": "agent:a", "request_id": "rename-tax-once" },
        "operations": [
            { "kind": "rename_symbol", "name": "tax", "new_name": "vat" }
        ]
    }));
    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles = (0..WRITERS)
        .map(|_| {
            let db_path = db_path.clone();
            let transaction = Arc::clone(&transaction);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut db = CodeDb::open(&db_path).unwrap();
                barrier.wait();
                workspace_call(&mut db, "ops.apply", (*transaction).clone())
            })
        })
        .collect::<Vec<_>>();

    let responses = handles
        .into_iter()
        .map(|handle| handle.join().expect("writer thread"))
        .collect::<Vec<_>>();

    // Every submission sees the same committed result, and the work happened once.
    let first = &responses[0];
    assert_eq!(first["status"], "ok");
    assert_eq!(first["result"]["committed"], true);
    for response in &responses {
        assert_eq!(response["status"], "ok");
        assert_eq!(response["result"], first["result"]);
        assert_eq!(response["snapshot"], first["snapshot"]);
    }
    assert_eq!(row_count(&db_path, "workspace_transactions"), 1);
    assert_eq!(row_count(&db_path, "migrations"), migrations_before + 1);
}
