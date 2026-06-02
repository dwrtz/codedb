use std::path::Path;
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::Value as JsonValue;
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
    assert!(stderr.contains("external symbols"));
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
