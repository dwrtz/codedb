// Phase 5 (SPEC_V3 §6) acceptance: a recursion-group object hashes
// deterministically, is referenced by the program root, survives the bundle
// export/import + migration-replay round-trip, and `verify` accepts a
// well-formed group while rejecting an inconsistent in-group reference.

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

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

const MUTUAL: &str = "fn is_even(n: i64) -> i64 = if n < 1 then 1 else is_odd(n - 1)\n\
                      fn is_odd(n: i64) -> i64 = if n < 1 then 0 else is_even(n - 1)\n\
                      fn main() -> i64 = is_even(8) + is_odd(8)\n";

fn import_root(db: &Path, source: &str) -> String {
    let temp = tempdir().unwrap();
    let src = temp.path().join("rec.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(db)]);
    let report = run(&["import", path(db), path(&src)]);
    report
        .lines()
        .find_map(|line| line.strip_prefix("root "))
        .expect("import prints root")
        .trim()
        .to_string()
}

/// Read the single `RecursionGroup` object's (hash, payload) from a database.
fn recursion_group(db: &Path) -> (String, JsonValue) {
    let conn = Connection::open(db).unwrap();
    let (hash, payload): (String, String) = conn
        .query_row(
            "SELECT hash, payload_json FROM objects WHERE kind = 'RecursionGroup'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("exactly one RecursionGroup object");
    (hash, serde_json::from_str(&payload).unwrap())
}

fn object_payload(db: &Path, hash: &str) -> JsonValue {
    let conn = Connection::open(db).unwrap();
    let payload: String = conn
        .query_row(
            "SELECT payload_json FROM objects WHERE hash = ?1",
            [hash],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&payload).unwrap()
}

#[test]
fn recursion_group_object_is_referenced_by_root_and_hashes_deterministically() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("rg.sqlite");
    let root = import_root(&db, MUTUAL);

    let (group_hash, group) = recursion_group(&db);
    let members = group["members"].as_array().expect("members array");
    assert_eq!(members.len(), 2, "is_even/is_odd clique has two members");
    for member in members {
        assert!(member["symbol"].as_str().unwrap().starts_with("sha256:"));
        assert!(member["definition"].as_str().unwrap().starts_with("sha256:"));
        assert!(member["signature"].as_str().unwrap().starts_with("sha256:"));
    }

    // The program root references the group.
    let root_payload = object_payload(&db, &root);
    let referenced: Vec<&str> = root_payload["recursion_groups"]
        .as_array()
        .expect("root has recursion_groups")
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(
        referenced.contains(&group_hash.as_str()),
        "root recursion_groups {referenced:?} references {group_hash}"
    );

    // Deterministic: re-importing the same source reproduces the same group hash
    // (member identities derive from the migration + in-group ordinal, SPEC_V3 §10).
    let db2 = temp.path().join("rg2.sqlite");
    import_root(&db2, MUTUAL);
    let (group_hash2, _) = recursion_group(&db2);
    assert_eq!(group_hash, group_hash2, "recursion-group hash is deterministic");
}

#[test]
fn recursion_group_survives_bundle_export_import() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source.sqlite");
    let imported_db = temp.path().join("imported.sqlite");
    let bundle = temp.path().join("rec.codedb.bundle");
    let root = import_root(&source_db, MUTUAL);
    let (group_hash, _) = recursion_group(&source_db);

    run(&[
        "bundle", "export", path(&source_db), "--root", &root, "--out", path(&bundle),
    ]);
    run(&["init", path(&imported_db)]);
    run(&["bundle", "import", path(&imported_db), path(&bundle)]);

    // The group object is in the imported closure and the program still verifies
    // and evaluates after migration replay.
    let (imported_group_hash, _) = recursion_group(&imported_db);
    assert_eq!(imported_group_hash, group_hash, "group hash preserved by bundle round-trip");
    bin()
        .args(["verify", path(&imported_db)])
        .assert()
        .success()
        .stdout("verify ok\n");
    assert_eq!(run(&["eval", path(&imported_db), "main"]).trim(), "1");
}

#[test]
fn verify_accepts_well_formed_recursion_group() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("ok.sqlite");
    import_root(&db, MUTUAL);
    bin()
        .args(["verify", path(&db)])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn verify_rejects_inconsistent_in_group_reference() {
    // Insert a RecursionGroup object whose first member claims a `definition`
    // that actually belongs to the OTHER member (its symbol disagrees). The
    // object is correctly hashed, so this exercises the recursion-group
    // consistency check, not the generic payload-hash check. `verify` scans every
    // stored object, so the inconsistent group is validated and rejected.
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad.sqlite");
    import_root(&db, MUTUAL);
    let (_, group) = recursion_group(&db);
    let members = group["members"].as_array().unwrap().clone();
    assert_eq!(members.len(), 2);

    // Swap member[0]'s definition for member[1]'s — now member[0].symbol points
    // at a FunctionDef whose own symbol is member[1].symbol.
    let mut bad_member0 = members[0].clone();
    bad_member0["definition"] = members[1]["definition"].clone();
    let bad_payload = serde_json::json!({
        "module": group["module"],
        "members": [bad_member0, members[1].clone()],
    });
    let canonical = canonical_json(&bad_payload);
    let hash = object_hash("RecursionGroup", &canonical);

    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO objects (hash, kind, schema_version, payload_json, payload_size_bytes)
         VALUES (?1, 'RecursionGroup', 1, ?2, ?3)",
        (&hash, &canonical, canonical.len() as i64),
    )
    .unwrap();

    let stderr = run_failure(&["verify", path(&db)]);
    assert!(
        stderr.contains("bad_recursion_group"),
        "expected bad_recursion_group, got: {stderr}"
    );
}

fn object_hash(kind: &str, canonical_payload: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/object/v1\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(b"1");
    hasher.update(b"\0");
    hasher.update(canonical_payload.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn canonical_json(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => serde_json::to_string(value).expect("string serialization"),
        JsonValue::Array(values) => {
            let inner = values
                .iter()
                .map(canonical_json)
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
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
    }
}
