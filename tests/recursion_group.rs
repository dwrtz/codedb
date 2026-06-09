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

#[test]
fn recursion_group_hash_is_source_order_independent() {
    // The clique's content identity is structural, not textual: declaring the same
    // mutually-recursive clique in two different source orders yields the SAME
    // recursion-group hash, because member ordinals (→ birth identities, SPEC_V3
    // §10) derive from the clique's structure, not source position.
    let even_first = "fn is_even(n: i64) -> i64 = if n < 1 then 1 else is_odd(n - 1)\n\
                      fn is_odd(n: i64) -> i64 = if n < 1 then 0 else is_even(n - 1)\n";
    let odd_first = "fn is_odd(n: i64) -> i64 = if n < 1 then 0 else is_even(n - 1)\n\
                     fn is_even(n: i64) -> i64 = if n < 1 then 1 else is_odd(n - 1)\n";
    let temp = tempdir().unwrap();
    let db1 = temp.path().join("even_first.sqlite");
    let db2 = temp.path().join("odd_first.sqlite");
    import_root(&db1, even_first);
    import_root(&db2, odd_first);
    let (hash1, _) = recursion_group(&db1);
    let (hash2, _) = recursion_group(&db2);
    assert_eq!(
        hash1, hash2,
        "recursion-group hash must be source-order-independent"
    );
}

#[test]
fn recursion_group_hash_is_canonical_for_a_symmetric_clique() {
    // Regression for the 1-WL incompleteness hole (SPEC_V3 §6): the two members have
    // BYTE-IDENTICAL bodies but call their peers in position-distinguishable argument
    // slots (`a` is always called with `n - 1`, `b` with `n - 2`). Colour refinement
    // (1-WL) cannot tell such members apart, so the old source-order tiebreak gave the
    // two source orderings DIFFERENT group/root hashes (and a non-fixpoint round-trip).
    // Individualization-refinement now assigns a canonical labeling, so both orderings
    // — and the export round-trip — agree. (is_even/is_odd cannot exercise this: their
    // differing base cases let 1-WL discretize them, so the tiebreak never fired.)
    let a_first = "fn a(n: i64) -> i64 = if n < 1 then 0 else a(n - 1) + b(n - 2)\n\
                   fn b(n: i64) -> i64 = if n < 1 then 0 else a(n - 1) + b(n - 2)\n\
                   fn main() -> i64 = a(6) + b(6)\n";
    let b_first = "fn b(n: i64) -> i64 = if n < 1 then 0 else a(n - 1) + b(n - 2)\n\
                   fn a(n: i64) -> i64 = if n < 1 then 0 else a(n - 1) + b(n - 2)\n\
                   fn main() -> i64 = a(6) + b(6)\n";
    let temp = tempdir().unwrap();
    let db1 = temp.path().join("a_first.sqlite");
    let db2 = temp.path().join("b_first.sqlite");
    let root1 = import_root(&db1, a_first);
    let root2 = import_root(&db2, b_first);
    let (group1, _) = recursion_group(&db1);
    let (group2, _) = recursion_group(&db2);

    assert_eq!(
        group1, group2,
        "symmetric-clique recursion-group hash must be source-order-independent"
    );
    assert_eq!(
        root1, root2,
        "symmetric-clique root hash must be source-order-independent"
    );
    // Semantics are preserved regardless of the canonical labeling chosen.
    assert_eq!(
        run(&["eval", path(&db1), "main"]).trim(),
        run(&["eval", path(&db2), "main"]).trim(),
    );

    // import → export → re-import is a fixpoint even when export renders the members
    // in a different (display) order than either source ordering.
    let export = temp.path().join("sym.export.cdb");
    run(&["export", path(&db1), "--branch", "main", "--out", path(&export)]);
    let db3 = temp.path().join("sym.rt.sqlite");
    run(&["init", path(&db3)]);
    let root3 = run(&["import", path(&db3), path(&export)])
        .lines()
        .find_map(|line| line.strip_prefix("root "))
        .expect("import prints root")
        .trim()
        .to_string();
    assert_eq!(
        root1, root3,
        "symmetric-clique root hash must round-trip through the projection"
    );
}

#[test]
fn recursion_group_projection_round_trip_is_a_fixpoint() {
    // import → export → re-import reproduces the SAME root hash and group hash: the
    // exported checked-view projection is identity-preserving even though its render
    // order may differ from the original source (the SPEC_V3 §11 round-trip gate).
    let temp = tempdir().unwrap();
    let db = temp.path().join("rt.sqlite");
    let export = temp.path().join("rt.export.cdb");
    let root1 = import_root(&db, MUTUAL);
    let (group1, _) = recursion_group(&db);

    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let db2 = temp.path().join("rt2.sqlite");
    run(&["init", path(&db2)]);
    let root2 = run(&["import", path(&db2), path(&export)])
        .lines()
        .find_map(|line| line.strip_prefix("root "))
        .expect("import prints root")
        .trim()
        .to_string();
    let (group2, _) = recursion_group(&db2);

    assert_eq!(root1, root2, "root hash must round-trip through the projection");
    assert_eq!(
        group1, group2,
        "recursion-group hash must round-trip through the projection"
    );
}

#[test]
fn verify_rejects_duplicate_recursion_group_member() {
    // A group that lists the same member symbol twice is malformed (the importer
    // mints exactly one ordinal per member, SPEC_V3 §10). Build a correctly-hashed
    // group whose members repeat member[0] and confirm `verify` rejects it.
    let temp = tempdir().unwrap();
    let db = temp.path().join("dup.sqlite");
    import_root(&db, MUTUAL);
    let (_, group) = recursion_group(&db);
    let members = group["members"].as_array().unwrap().clone();

    let bad_payload = serde_json::json!({
        "module": group["module"],
        "members": [members[0].clone(), members[0].clone()],
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
        stderr.contains("more than once"),
        "expected duplicate-member rejection, got: {stderr}"
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
