use std::path::Path;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

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

fn test_bytes_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/bytes/v1\0");
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
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
