use std::path::Path;
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

// Phase 9 coverage map:
// - object store: payload tampering, missing objects, recomputed object_edges
// - materialized indexes: root_symbols/root_names/root_exports/dependencies
// - histories: bad migration/history links and read-only replay checks
// - caches/artifacts: cache key JSON, artifact metadata, bytes hashes, missing bytes
// - native artifacts: object metadata, link plans, executable metadata
// - projection runtime boundary: forbidden C runtime calls in cached projections

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn run_failure(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
}

fn setup_shop(db: &Path) {
    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
}

#[test]
fn clean_example_databases_pass_verification() {
    for example in [
        "examples/shop.cdb",
        "examples/discount.cdb",
        "examples/booleans.cdb",
    ] {
        let temp = tempdir().unwrap();
        let db = temp.path().join("clean.sqlite");
        run(&["init", db.to_str().unwrap()]);
        run(&["import", db.to_str().unwrap(), example]);

        bin()
            .args(["verify", db.to_str().unwrap()])
            .assert()
            .success()
            .stdout("verify ok\n");
    }
}

#[test]
fn verify_rejects_object_payload_tampering() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("tampered-payload.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    let (hash, payload_json): (String, String) = conn
        .query_row(
            "SELECT hash, payload_json FROM objects
             WHERE kind = 'Expression' AND payload_json LIKE '%literal_i64%'
             ORDER BY hash LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut payload: JsonValue = serde_json::from_str(&payload_json).unwrap();
    payload["value"] = JsonValue::String("999".to_string());
    let canonical = test_canonical_json(&payload);
    conn.execute(
        "UPDATE objects SET payload_json = ?1, payload_size_bytes = ?2 WHERE hash = ?3",
        (&canonical, canonical.len() as i64, hash),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_hash"));
}

#[test]
fn verify_rejects_missing_objects_referenced_by_payloads() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("missing-object.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    conn.execute(
        "DELETE FROM objects
         WHERE hash = (
            SELECT e.child_hash
            FROM object_edges e
            JOIN objects child ON child.hash = e.child_hash
            WHERE child.kind != 'Type'
            ORDER BY e.parent_hash, e.child_hash
            LIMIT 1
         )",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("missing_object"));
}

#[test]
fn verify_rejects_missing_object_references_even_without_edges() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("missing-payload-ref.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    let payload_json: String = conn
        .query_row(
            "SELECT payload_json FROM objects
             WHERE kind = 'FunctionDef'
             ORDER BY hash LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut payload: JsonValue = serde_json::from_str(&payload_json).unwrap();
    payload["typed_body_expr_hash"] = JsonValue::String(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    );
    let canonical = test_canonical_json(&payload);
    let hash = test_object_hash("FunctionDef", &canonical);
    conn.execute(
        "INSERT INTO objects (hash, kind, schema_version, payload_json, payload_size_bytes)
         VALUES (?1, 'FunctionDef', 1, ?2, ?3)",
        (&hash, &canonical, canonical.len() as i64),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("missing_object"));
    assert!(stderr.contains("typed_body_expr_hash"));
}

#[test]
fn verify_recomputes_object_edges_from_payload_references() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-edges.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "DELETE FROM object_edges
         WHERE rowid = (SELECT rowid FROM object_edges ORDER BY parent_hash, child_hash LIMIT 1)",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_index: object_edges mismatch"));
}

#[test]
fn verify_rejects_bad_dependency_indexes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-dependencies.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "DELETE FROM dependencies
         WHERE rowid = (SELECT rowid FROM dependencies ORDER BY root_hash LIMIT 1)",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_dependency_index"));
}

#[test]
fn verify_rejects_bad_root_name_indexes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-root-names.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "DELETE FROM root_names
         WHERE rowid = (SELECT rowid FROM root_names ORDER BY root_hash LIMIT 1)",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_index: root_names mismatch"));
}

#[test]
fn verify_rejects_bad_root_export_indexes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-root-exports.sqlite");
    setup_shop(&db);
    run(&["set-export", db.to_str().unwrap(), "tax", "public_tax"]);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "DELETE FROM root_exports
         WHERE rowid = (SELECT rowid FROM root_exports ORDER BY root_hash LIMIT 1)",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_index: root_exports mismatch"));
}

#[test]
fn verify_rejects_missing_main_branch_without_repairing_it() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("missing-main-branch.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM branches WHERE name = 'main'", [])
        .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_index: main branch is missing"));
    assert_eq!(table_row_count(&db, "branches"), 0);
}

#[test]
fn verify_rejects_bad_source_search_index() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-source-search.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM source_search", []).unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_index: source_search mismatch"));
}

#[test]
fn verify_rejects_duplicate_source_search_rows() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("duplicate-source-search.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO source_search (root_hash, symbol_hash, rendered_source)
         SELECT root_hash, symbol_hash, rendered_source
         FROM source_search
         LIMIT 1",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_index: source_search mismatch"));
}

#[test]
fn verify_rejects_bad_history_hashes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-history.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE histories
         SET output_root_hash = ?1
         WHERE rowid = (SELECT rowid FROM histories ORDER BY created_at LIMIT 1)",
        ["sha256:0000000000000000000000000000000000000000000000000000000000000000"],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_history_link"));
}

#[test]
fn verify_rejects_migration_operation_kind_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("migration-kind-mismatch.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE migrations
         SET operation_kind = 'rename_symbol'
         WHERE rowid = (SELECT rowid FROM migrations ORDER BY created_at, hash LIMIT 1)",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_history_link"));
    assert!(
        stderr.contains("operation kind rename_symbol does not match operation create_function")
    );
}

#[test]
fn verify_rejects_history_output_that_disagrees_with_migration_output() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("history-output-mismatch.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let old_history_hash: String = conn
        .query_row(
            "SELECT history_hash FROM branches WHERE name = 'main'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (parent_history, old_migration_hash, history_output): (Option<String>, String, String) =
        conn.query_row(
            "SELECT parent_history_hash, migration_hash, output_root_hash
             FROM histories WHERE history_hash = ?1",
            [&old_history_hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let (input_root, operation_json, preconditions_json, postconditions_json): (
        String,
        String,
        String,
        String,
    ) = conn
        .query_row(
            "SELECT input_root_hash, operation_json, preconditions_json, postconditions_json
             FROM migrations WHERE hash = ?1",
            [&old_migration_hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_ne!(input_root, history_output);
    let operation = serde_json::from_str::<JsonValue>(&operation_json).unwrap();
    let preconditions = serde_json::from_str::<JsonValue>(&preconditions_json).unwrap();
    let postconditions = serde_json::from_str::<JsonValue>(&postconditions_json).unwrap();
    let new_migration_hash = test_migration_hash(
        parent_history.as_deref(),
        &input_root,
        &input_root,
        &operation,
        &preconditions,
        &postconditions,
    );
    let new_history_hash = test_history_hash(
        parent_history.as_deref(),
        &new_migration_hash,
        &history_output,
    );

    conn.execute(
        "UPDATE migrations
         SET hash = ?1, output_root_hash = ?2
         WHERE hash = ?3",
        (&new_migration_hash, &input_root, &old_migration_hash),
    )
    .unwrap();
    conn.execute(
        "UPDATE histories
         SET history_hash = ?1, migration_hash = ?2
         WHERE history_hash = ?3",
        (&new_history_hash, &new_migration_hash, &old_history_hash),
    )
    .unwrap();
    conn.execute(
        "UPDATE branches SET history_hash = ?1 WHERE name = 'main'",
        [&new_history_hash],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_history_link"));
    assert!(stderr.contains("does not match migration"));
}

#[test]
fn verify_rejects_branch_history_head_that_outputs_a_different_root() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("branch-history-root-mismatch.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let previous_root: String = conn
        .query_row(
            "SELECT m.input_root_hash
             FROM branches b
             JOIN histories h ON h.history_hash = b.history_hash
             JOIN migrations m ON m.hash = h.migration_hash
             WHERE b.name = 'main'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE branches SET root_hash = ?1 WHERE name = 'main'",
        [&previous_root],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_history_link"));
    assert!(stderr.contains("branch histories output wrong root"));
}

#[test]
fn verify_rejects_malformed_workspace_transaction_response() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-workspace-transaction.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    let root_hash: String = conn
        .query_row(
            "SELECT root_hash FROM branches WHERE name = 'main'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO workspace_transactions
         (request_id, request_hash, method, branch, expected_root_hash, response_json)
         VALUES ('bad-request', 'sha256:request', 'ops.apply', 'main', ?1, 'not-json')",
        [&root_hash],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_workspace_transaction"));
    assert!(stderr.contains("response_json is invalid JSON"));
}

#[test]
fn verify_allows_succeeded_artifact_job_without_disposable_cache_entry() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("artifact-job-missing-cache.sqlite");
    let plan = temp.path().join("main.link.json");
    setup_shop(&db);
    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO artifact_jobs
         (cache_key, artifact_kind, status, worker_id, started_at, finished_at)
         VALUES (
           'sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff',
           'canonical_source',
           'succeeded',
           'worker:test',
           CURRENT_TIMESTAMP,
           CURRENT_TIMESTAMP
         )",
        [],
    )
    .unwrap();

    run(&["verify", db.to_str().unwrap()]);
}

#[test]
fn verify_rejects_malformed_artifact_job_error() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-artifact-job-error.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO artifact_jobs
         (cache_key, artifact_kind, status, worker_id, started_at, finished_at, error_json)
         VALUES ('sha256:bad-job', 'object_file', 'failed', 'worker:test', 'started', 'finished', '{}')",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_artifact_job"));
    assert!(stderr.contains("error_json schema mismatch"));
    assert!(stderr.contains("error_json missing kind"));
    assert!(stderr.contains("error_json missing message"));
}

#[test]
fn verify_replay_does_not_repair_corrupt_indexes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("verify-readonly.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "DELETE FROM root_symbols WHERE rowid = (SELECT rowid FROM root_symbols LIMIT 1)",
        [],
    )
    .unwrap();

    let first = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(first.contains("bad_index: root_symbols mismatch"));
    let second = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(second.contains("bad_index: root_symbols mismatch"));
}

#[test]
fn verify_rejects_unknown_cache_artifact_kinds() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("unknown-cache-kind.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE compile_cache
         SET artifact_kind = 'unknown_artifact'
         WHERE cache_key = (SELECT cache_key FROM compile_cache ORDER BY cache_key LIMIT 1)",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_cache_entry"));
    assert!(stderr.contains("unknown artifact kind"));
}

#[test]
fn verify_rejects_noncanonical_signature_effects() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("noncanonical-effects.sqlite");
    run(&["init", db.to_str().unwrap()]);

    let i64_hash = test_object_hash("Type", r#"{"type_kind":"I64"}"#);
    let payload = json!({
        "abi": "codedb-v0-internal",
        "effects": ["io", "io"],
        "params": [],
        "return": i64_hash,
    });
    let canonical = test_canonical_json(&payload);
    let signature_hash = test_object_hash("FunctionSignature", &canonical);
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO objects (hash, kind, schema_version, payload_json, payload_size_bytes)
         VALUES (?1, 'FunctionSignature', 1, ?2, ?3)",
        (&signature_hash, &canonical, canonical.len() as i64),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO object_edges (parent_hash, child_hash, edge_label, edge_position)
         VALUES (?1, ?2, 'ref', 0)",
        (&signature_hash, &i64_hash),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_signature_effects"), "{stderr}");
    assert!(stderr.contains("not canonical"), "{stderr}");
}

#[test]
fn verify_rejects_noncanonical_structural_type_payloads() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("noncanonical-type.sqlite");
    run(&["init", db.to_str().unwrap()]);

    let i64_hash = test_object_hash("Type", r#"{"type_kind":"I64"}"#);
    let payload = json!({
        "type_kind": "Record",
        "fields": [
            { "name": "tax", "type": i64_hash },
            { "name": "amount", "type": i64_hash },
        ],
    });
    let canonical = test_canonical_json(&payload);
    let type_hash = test_object_hash("Type", &canonical);
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO objects (hash, kind, schema_version, payload_json, payload_size_bytes)
         VALUES (?1, 'Type', 1, ?2, ?3)",
        (&type_hash, &canonical, canonical.len() as i64),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO object_edges (parent_hash, child_hash, edge_label, edge_position)
         VALUES (?1, ?2, 'ref', 0)",
        (&type_hash, &i64_hash),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_type_object"), "{stderr}");
    assert!(stderr.contains("not canonical"), "{stderr}");
}

#[test]
fn verify_rejects_cache_key_payload_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("cache-mismatch.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, cache_key_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, cache_key_json FROM compile_cache
             WHERE artifact_kind = 'interface_hash'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&cache_key_json).unwrap();
    value["target_triple"] = JsonValue::String("aarch64-apple-darwin".to_string());
    conn.execute(
        "UPDATE compile_cache SET cache_key_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_cache_entry"));
    assert!(stderr.contains("cache key mismatch"));
}

#[test]
fn verify_rejects_implementation_hash_metadata_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("implementation-metadata-mismatch.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'implementation_hash'
             ORDER BY cache_key",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    let (cache_key, mut value) = rows
        .map(|row| {
            let (cache_key, artifact_json) = row.unwrap();
            let value = serde_json::from_str::<JsonValue>(&artifact_json).unwrap();
            (cache_key, value)
        })
        .find(|(_, value)| {
            value["metadata"]["direct_dependency_interface_hashes"]
                .as_array()
                .is_some_and(|hashes| !hashes.is_empty())
        })
        .expect("implementation cache entry with dependencies");

    value["metadata"]["direct_dependency_interface_hashes"] = serde_json::json!([]);
    let metadata_hash = test_metadata_hash(&value["metadata"]);
    value["metadata_hash"] = JsonValue::String(metadata_hash.clone());
    conn.execute(
        "UPDATE compile_cache
         SET artifact_json = ?1, artifact_hash = ?2
         WHERE cache_key = ?3",
        (test_canonical_json(&value), metadata_hash, cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_implementation_hash"));
    assert!(stderr.contains("dependency interface metadata does not match cache key"));
}

#[test]
fn verify_rejects_lowered_ir_that_does_not_match_semantic_dag() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("lowered-ir-mismatch.sqlite");
    let ir = temp.path().join("main.ir.json");
    setup_shop(&db);
    run(&[
        "emit-ir",
        db.to_str().unwrap(),
        "main",
        "--out",
        ir.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'lowered_ir'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    let operations = value["metadata"]["operations"].as_array_mut().unwrap();
    let const_i64 = operations
        .iter_mut()
        .find(|op| op["op"].as_str() == Some("const_i64"))
        .expect("const_i64 operation");
    const_i64["value"] = JsonValue::String("999".to_string());
    let metadata_hash = test_metadata_hash(&value["metadata"]);
    value["metadata_hash"] = JsonValue::String(metadata_hash.clone());
    conn.execute(
        "UPDATE compile_cache
         SET artifact_json = ?1, artifact_hash = ?2
         WHERE cache_key = ?3",
        (test_canonical_json(&value), metadata_hash, cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_lowered_ir"));
    assert!(stderr.contains("does not match recomputed semantic DAG"));

    run(&[
        "emit-ir",
        db.to_str().unwrap(),
        "main",
        "--out",
        ir.to_str().unwrap(),
    ]);
    let repaired_ir = std::fs::read_to_string(&ir).unwrap();
    assert!(!repaired_ir.contains("\"value\": \"999\""));
    assert!(repaired_ir.contains("\"value\": \"100\""));
}

#[test]
fn verify_rejects_artifact_metadata_backend_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("artifact-metadata-mismatch.sqlite");
    let c_file = temp.path().join("projection.c");
    setup_shop(&db);
    run(&[
        "emit-c",
        db.to_str().unwrap(),
        "main",
        "--out",
        c_file.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'c_projection'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["backend_id"] = JsonValue::String("wrong-backend".to_string());
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_cache_entry"));
    assert!(stderr.contains("artifact metadata backend mismatch"));
}

#[test]
fn verify_rejects_malformed_native_artifact_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("malformed-artifact.sqlite");
    let object = temp.path().join("tax.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        object.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["relocations"] = JsonValue::String("not-an-array".to_string());
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("relocations must be an array"));
}

#[test]
fn verify_rejects_native_object_metadata_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("object-metadata-mismatch.sqlite");
    let tax_obj = temp.path().join("tax.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        tax_obj.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["target_triple"] = JsonValue::String("bad-target".to_string());
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("target"));
}

#[test]
fn verify_rejects_native_object_metadata_that_disagrees_with_function_def() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("object-function-def-mismatch.sqlite");
    let tax_obj = temp.path().join("tax.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        tax_obj.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["typed_body_expr_hash"] = JsonValue::String(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    );
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("typed body metadata does not match FunctionDef"));
}

#[test]
fn verify_rejects_mismatched_object_bytes_hashes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-object-bytes.sqlite");
    let object = temp.path().join("tax.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        object.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE compile_cache
         SET artifact_bytes = ?1
         WHERE artifact_kind = 'object_file'
           AND cache_key = (
             SELECT cache_key FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1
           )",
        [b"\x7fELFcorrupt".as_slice()],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_artifact_bytes"));
}

#[test]
fn verify_rejects_native_object_bytes_that_do_not_match_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("fake-object-bytes.sqlite");
    let object = temp.path().join("tax.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        object.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let fake_bytes = b"\x7fELFnot-real".to_vec();
    let fake_hash = test_bytes_hash(&fake_bytes);
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["bytes_hash"] = JsonValue::String(fake_hash.clone());
    conn.execute(
        "UPDATE compile_cache
         SET artifact_bytes = ?1, artifact_hash = ?2, artifact_json = ?3
         WHERE cache_key = ?4",
        (
            fake_bytes.as_slice(),
            &fake_hash,
            test_canonical_json(&value),
            cache_key,
        ),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("object bytes are not valid ELF"));
}

#[test]
fn verify_rejects_native_object_text_bytes_that_match_updated_hash() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("tampered-object-text.sqlite");
    let object = temp.path().join("tax.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        object.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json, mut object_bytes): (String, String, Vec<u8>) = conn
        .query_row(
            "SELECT cache_key, artifact_json, artifact_bytes FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(&object_bytes[..4], b"\x7fELF");
    assert!(object_bytes.len() > 64);
    object_bytes[64] ^= 0xff;
    let updated_hash = test_bytes_hash(&object_bytes);
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["bytes_hash"] = JsonValue::String(updated_hash.clone());
    conn.execute(
        "UPDATE compile_cache
         SET artifact_bytes = ?1, artifact_hash = ?2, artifact_json = ?3
         WHERE cache_key = ?4",
        (
            object_bytes.as_slice(),
            &updated_hash,
            test_canonical_json(&value),
            cache_key,
        ),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("deterministic native backend output"));
}

#[test]
fn verify_rejects_native_object_dependency_metadata_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp
        .path()
        .join("object-dependency-metadata-mismatch.sqlite");
    let object = temp.path().join("total.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        object.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["called_symbols"] = serde_json::json!([]);
    value["metadata"]["relocations"] = serde_json::json!([]);
    value["metadata"]["dependency_closure"] = serde_json::json!([]);
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("called_symbols metadata does not match any indexed root"));
}

#[test]
fn verify_rejects_native_object_dependency_interfaces_that_match_tampered_cache_key() {
    let temp = tempdir().unwrap();
    let db = temp
        .path()
        .join("object-dependency-interface-mismatch.sqlite");
    let object = temp.path().join("total.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        object.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, cache_key_json, artifact_json): (String, String, String) = conn
        .query_row(
            "SELECT cache_key, cache_key_json, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let mut key_value: JsonValue = serde_json::from_str(&cache_key_json).unwrap();
    let mut artifact_value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    key_value["dependency_interface_hashes"] = serde_json::json!([]);
    artifact_value["metadata"]["dependency_interface_hashes"] = serde_json::json!([]);
    let new_cache_key = test_cache_hash(&key_value);
    conn.execute(
        "UPDATE compile_cache
         SET cache_key = ?1, cache_key_json = ?2, artifact_json = ?3
         WHERE cache_key = ?4",
        (
            &new_cache_key,
            test_canonical_json(&key_value),
            test_canonical_json(&artifact_value),
            cache_key,
        ),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("dependency interface metadata does not match indexed root"));
}

#[test]
fn verify_rejects_missing_duplicate_relocation_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("duplicate-relocation-metadata.sqlite");
    let source = temp.path().join("two-calls.cdb");
    let object = temp.path().join("main.o");
    std::fs::write(
        &source,
        "fn inc(x: i64) -> i64 = x + 1\n\nfn main() -> i64 = inc(1) + inc(2)\n",
    )
    .unwrap();
    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), source.to_str().unwrap()]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        object.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    let (cache_key, mut value) = rows
        .map(|row| {
            let (cache_key, artifact_json) = row.unwrap();
            let value = serde_json::from_str::<JsonValue>(&artifact_json).unwrap();
            (cache_key, value)
        })
        .find(|(_, value)| {
            value["metadata"]["relocations"]
                .as_array()
                .is_some_and(|relocations| relocations.len() == 2)
        })
        .expect("object with duplicate call relocations");
    value["metadata"]["relocations"]
        .as_array_mut()
        .unwrap()
        .truncate(1);
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("relocations do not match lowered call sites"));
}

#[test]
fn verify_rejects_missing_native_object_bytes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("missing-object-bytes.sqlite");
    let object = temp.path().join("tax.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        object.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE compile_cache
         SET artifact_bytes = NULL
         WHERE artifact_kind = 'object_file'
           AND cache_key = (
             SELECT cache_key FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1
           )",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_artifact_bytes"));
    assert!(stderr.contains("missing artifact bytes"));
}

#[test]
fn verify_rejects_c_projection_forbidden_runtime_calls() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("forbidden-runtime.sqlite");
    let c_file = temp.path().join("projection.c");
    setup_shop(&db);
    run(&[
        "emit-c",
        db.to_str().unwrap(),
        "main",
        "--out",
        c_file.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'c_projection'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    let text = "void codedb_bad(void) { malloc(); }\n";
    let text_hash = test_bytes_hash(text.as_bytes());
    value["text"] = JsonValue::String(text.to_string());
    value["text_hash"] = JsonValue::String(text_hash.clone());
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1, artifact_hash = ?2 WHERE cache_key = ?3",
        (test_canonical_json(&value), text_hash, cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("forbidden_runtime_dependency"));
    assert!(stderr.contains("malloc"));
}

#[test]
fn verify_rejects_cached_link_plan_metadata_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("link-plan-metadata-mismatch.sqlite");
    let plan_path = temp.path().join("main.link.json");
    setup_shop(&db);
    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_path.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'link_plan'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["external_symbols"] = serde_json::json!(["puts"]);
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_link_plan"));
    assert!(stderr.contains("external symbol missing symbol"));
}

#[test]
fn verify_rejects_link_plan_object_metadata_that_disagrees_with_cached_object() {
    let temp = tempdir().unwrap();
    let db = temp
        .path()
        .join("link-plan-object-metadata-mismatch.sqlite");
    let plan_path = temp.path().join("main.link.json");
    setup_shop(&db);
    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_path.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    drop(stmt);
    let (cache_key, mut value) = rows
        .into_iter()
        .find_map(|(cache_key, artifact_json)| {
            let value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
            let has_relocations = value["metadata"]["relocations"]
                .as_array()
                .is_some_and(|relocations| !relocations.is_empty());
            has_relocations.then_some((cache_key, value))
        })
        .expect("object with relocations");
    value["metadata"]["relocations"] = serde_json::json!([]);
    value["metadata"]["called_symbols"] = serde_json::json!([]);
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_link_plan"));
    assert!(stderr.contains("does not match object artifact metadata"));
}

#[test]
fn verify_rejects_link_plan_that_cannot_be_recomputed_from_a_root() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("link-plan-root-mismatch.sqlite");
    let plan_path = temp.path().join("main.link.json");
    setup_shop(&db);
    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_path.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'link_plan'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["objects"][0]["definition_hash"] = JsonValue::String(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    );
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_link_plan"));
    assert!(stderr.contains("cannot be recomputed from any indexed root"));
}

#[test]
fn verify_rejects_link_plans_that_reference_missing_object_artifacts() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("missing-link-object.sqlite");
    let plan = temp.path().join("main.link.json");
    setup_shop(&db);
    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "DELETE FROM compile_cache
         WHERE artifact_kind = 'object_file'
           AND cache_key = (
             SELECT cache_key FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1
           )",
        [],
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_link_plan"));
    assert!(stderr.contains("references missing object"));
}

#[test]
fn verify_rejects_executable_metadata_that_loses_link_plan_dependency() {
    if !can_build_default_native_target() {
        return;
    }

    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-executable-metadata.sqlite");
    let executable = temp.path().join("shop");
    setup_shop(&db);
    run(&[
        "build",
        db.to_str().unwrap(),
        "main",
        "--out",
        executable.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'executable'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["link_plan_hash"] = JsonValue::String(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    );
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_executable_artifact"));
    assert!(stderr.contains("missing link plan dependency"));
}

fn test_bytes_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/bytes/v1\0");
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn test_cache_hash(value: &JsonValue) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/cache/v1\0");
    hasher.update(test_canonical_json(value).as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn test_metadata_hash(value: &JsonValue) -> String {
    test_bytes_hash(test_canonical_json(value).as_bytes())
}

fn test_object_hash(kind: &str, canonical_payload: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/object/v1\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(b"1");
    hasher.update(b"\0");
    hasher.update(canonical_payload.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn test_migration_hash(
    parent_history_hash: Option<&str>,
    input_root_hash: &str,
    output_root_hash: &str,
    operation: &JsonValue,
    preconditions: &JsonValue,
    postconditions: &JsonValue,
) -> String {
    let payload = json!({
        "parent_history_hash": parent_history_hash,
        "input_root_hash": input_root_hash,
        "output_root_hash": output_root_hash,
        "operation": operation,
        "preconditions": preconditions,
        "postconditions": postconditions,
    });
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/migration/v1\0");
    hasher.update(test_canonical_json(&payload).as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn test_history_hash(
    parent_history_hash: Option<&str>,
    migration_hash: &str,
    output_root_hash: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/history/v1\0");
    hasher.update(parent_history_hash.unwrap_or("").as_bytes());
    hasher.update([0]);
    hasher.update(migration_hash.as_bytes());
    hasher.update([0]);
    hasher.update(output_root_hash.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn table_row_count(db: &Path, table: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && ProcessCommand::new("cc").arg("--version").output().is_ok()
}

fn test_canonical_json(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => serde_json::to_string(value).expect("string serialization"),
        JsonValue::Array(values) => {
            let inner = values
                .iter()
                .map(test_canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        JsonValue::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key serialization"),
                        test_canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
    }
}

#[test]
fn index_root_prunes_orphan_lowered_ir_cache_entries() {
    // A lowered-IR cache entry whose function definition is in no indexed root
    // would otherwise receive only shape-level verification. index_root prunes
    // such orphans so verify never audits an entry that can never be served for
    // a live build. (Cache contents are not hashed, so this does not affect root
    // identity or replay determinism.)
    let temp = tempdir().unwrap();
    let db = temp.path().join("orphan-cache.sqlite");
    setup_shop(&db);

    let conn = Connection::open(&db).unwrap();
    // Any stored object that is not a function definition in any root makes a
    // valid (FK-satisfying) but orphaned cache input.
    let orphan_input: String = conn
        .query_row(
            "SELECT hash FROM objects
             WHERE hash NOT IN (SELECT definition_hash FROM root_symbols)
             ORDER BY hash LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO compile_cache
         (cache_key, cache_key_json, input_hash, backend, target, compiler_version,
          artifact_kind, artifact_hash, artifact_json)
         VALUES ('orphan-test-key', '{}', ?1, 'test-backend', 'test-target',
                 'test-compiler', 'lowered_ir', 'sha256:orphan', '{}')",
        [&orphan_input],
    )
    .unwrap();
    let before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM compile_cache WHERE cache_key = 'orphan-test-key'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, 1);
    drop(conn);

    // Any migration re-indexes a root and runs the prune.
    let listing: JsonValue =
        serde_json::from_str(&run(&["list", db.to_str().unwrap(), "--json"])).unwrap();
    let root = listing["root_hash"].as_str().unwrap().to_string();
    run(&[
        "replace-body",
        db.to_str().unwrap(),
        "total",
        "subtotal + 2",
        "--expect-root",
        &root,
        "--json",
    ]);

    let conn = Connection::open(&db).unwrap();
    let after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM compile_cache WHERE cache_key = 'orphan-test-key'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, 0, "orphan lowered-IR cache entry should be pruned");
}
