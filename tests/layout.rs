use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
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

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn type_layouts_are_deterministic_for_records_refs_arrays_and_enums() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("layout.sqlite");
    let source = temp.path().join("layout.cdb");
    let line_view_layout = temp.path().join("line-view-layout.json");
    let cursor_layout = temp.path().join("cursor-layout.json");
    let batch_layout = temp.path().join("batch-layout.json");
    let discount_layout = temp.path().join("discount-layout.json");
    let raw_slot_layout = temp.path().join("raw-slot-layout.json");
    let projection = temp.path().join("layout.projection.cdb");
    let rebuilt = temp.path().join("rebuilt.sqlite");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineView<'a> {
  line: &'a Line
}

record Cursor<'a> {
  line: &'a mut Line
  pos: i64
}

record Batch {
  values: array<i64, 4>
  flag: bool
}

record RawSlot {
  ptr: raw_ptr<i64>
  mut_ptr: raw_mut_ptr<Line>
}

enum Discount {
  none: unit
  percent: i64
  batch: Batch
}

fn main() -> i64 = 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "emit-type-layout",
        path(&db),
        "LineView",
        "--out",
        path(&line_view_layout),
    ]);
    run(&[
        "emit-type-layout",
        path(&db),
        "Cursor",
        "--out",
        path(&cursor_layout),
    ]);
    run(&[
        "emit-type-layout",
        path(&db),
        "Batch",
        "--out",
        path(&batch_layout),
    ]);
    run(&[
        "emit-type-layout",
        path(&db),
        "Discount",
        "--out",
        path(&discount_layout),
    ]);
    run(&[
        "emit-type-layout",
        path(&db),
        "RawSlot",
        "--out",
        path(&raw_slot_layout),
    ]);
    run(&["verify", path(&db)]);
    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("line: &'a Line"));
    assert!(exported.contains("values: array<i64, 4>"));
    assert!(exported.contains("ptr: raw_ptr<i64>"));
    assert!(exported.contains("mut_ptr: raw_mut_ptr<Line>"));
    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    run(&["verify", path(&rebuilt)]);

    let line_view = read_json(&line_view_layout);
    assert_eq!(line_view["schema"], "codedb/type-layout/v2");
    assert_eq!(line_view["layout_version"], "layout:v2");
    assert_eq!(line_view["kind"], "record");
    assert_eq!(line_view["size_bytes"], 8);
    assert_eq!(line_view["align_bytes"], 8);
    assert_eq!(line_view["copy_kind"], "copy");
    assert_eq!(line_view["drop_kind"], "trivial");
    assert_eq!(line_view["abi"]["pass"], "by_value");
    assert_eq!(line_view["abi"]["return"], "by_value");
    assert_eq!(line_view["contains_reference"], true);
    assert_eq!(line_view["contains_mut_reference"], false);
    assert_eq!(line_view["fields"][0]["name"], "line");
    assert_eq!(line_view["fields"][0]["offset_bytes"], 0);
    assert_eq!(line_view["fields"][0]["size_bytes"], 8);
    assert_eq!(line_view["fields"][0]["align_bytes"], 8);

    let cursor = read_json(&cursor_layout);
    assert_eq!(cursor["kind"], "record");
    assert_eq!(cursor["size_bytes"], 16);
    assert_eq!(cursor["align_bytes"], 8);
    assert_eq!(cursor["copy_kind"], "move_only");
    assert_eq!(cursor["contains_reference"], true);
    assert_eq!(cursor["contains_mut_reference"], true);
    assert_eq!(cursor["fields"][0]["name"], "line");
    assert_eq!(cursor["fields"][0]["offset_bytes"], 0);
    assert_eq!(cursor["fields"][1]["name"], "pos");
    assert_eq!(cursor["fields"][1]["offset_bytes"], 8);

    let batch = read_json(&batch_layout);
    assert_eq!(batch["kind"], "record");
    assert_eq!(batch["size_bytes"], 40);
    assert_eq!(batch["align_bytes"], 8);
    assert_eq!(batch["abi"]["pass"], "by_indirect");
    assert_eq!(batch["abi"]["return"], "hidden_return_slot");
    assert_eq!(batch["fields"][0]["name"], "values");
    assert_eq!(batch["fields"][0]["offset_bytes"], 0);
    assert_eq!(batch["fields"][0]["size_bytes"], 32);
    assert_eq!(batch["fields"][1]["name"], "flag");
    assert_eq!(batch["fields"][1]["offset_bytes"], 32);

    let discount = read_json(&discount_layout);
    assert_eq!(discount["kind"], "enum");
    assert_eq!(discount["size_bytes"], 48);
    assert_eq!(discount["align_bytes"], 8);
    assert_eq!(discount["abi"]["pass"], "by_indirect");
    assert_eq!(discount["abi"]["return"], "hidden_return_slot");
    assert_eq!(discount["tag"]["offset_bytes"], 0);
    assert_eq!(discount["tag"]["size_bytes"], 8);
    assert_eq!(discount["payload_offset_bytes"], 8);
    assert_eq!(discount["payload_size_bytes"], 40);
    assert_eq!(discount["variants"][0]["name"], "none");
    assert_eq!(discount["variants"][0]["tag_value"], 0);
    assert_eq!(discount["variants"][1]["name"], "percent");
    assert_eq!(discount["variants"][1]["payload_size_bytes"], 8);
    assert_eq!(discount["variants"][2]["name"], "batch");
    assert_eq!(discount["variants"][2]["payload_size_bytes"], 40);

    let raw_slot = read_json(&raw_slot_layout);
    assert_eq!(raw_slot["kind"], "record");
    assert_eq!(raw_slot["size_bytes"], 16);
    assert_eq!(raw_slot["align_bytes"], 8);
    assert_eq!(raw_slot["copy_kind"], "copy");
    assert_eq!(raw_slot["contains_raw_pointer"], true);
    assert_eq!(raw_slot["fields"][0]["name"], "ptr");
    assert_eq!(raw_slot["fields"][0]["offset_bytes"], 0);
    assert_eq!(raw_slot["fields"][1]["name"], "mut_ptr");
    assert_eq!(raw_slot["fields"][1]["offset_bytes"], 8);

    let conn = Connection::open(&db).unwrap();
    let (cache_key_json, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key_json, artifact_json
             FROM compile_cache
             WHERE artifact_kind = 'type_layout'
             ORDER BY created_at
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let cache_key: JsonValue = serde_json::from_str(&cache_key_json).unwrap();
    assert_eq!(cache_key["artifact_kind"], "type_layout");
    assert_eq!(cache_key["backend_id"], "type-layout:v2");
    assert_eq!(cache_key["target_triple"], codedb::DEFAULT_NATIVE_TARGET);
    let artifact: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    assert_eq!(artifact["metadata"]["layout_version"], "layout:v2");
}

#[test]
fn verify_recomputes_and_rejects_malformed_type_layout_artifact() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-layout.sqlite");
    let source = temp.path().join("line.cdb");
    let layout_path = temp.path().join("line-layout.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn main() -> i64 = 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "emit-type-layout",
        path(&db),
        "Line",
        "--out",
        path(&layout_path),
    ]);
    run(&["verify", path(&db)]);

    corrupt_first_type_layout_size(&db, 24);

    bin()
        .args(["verify", path(&db)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_type_layout"));
}

#[test]
fn verify_recomputes_and_rejects_malformed_type_layout_abi_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-layout-abi.sqlite");
    let source = temp.path().join("line.cdb");
    let layout_path = temp.path().join("line-layout.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn main() -> i64 = 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "emit-type-layout",
        path(&db),
        "Line",
        "--out",
        path(&layout_path),
    ]);
    run(&["verify", path(&db)]);

    corrupt_first_type_layout_abi(&db, "by_value", "by_value");

    bin()
        .args(["verify", path(&db)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_type_layout"));
}

fn read_json(path: &Path) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn corrupt_first_type_layout_size(db: &Path, size_bytes: u64) {
    let conn = Connection::open(db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json
             FROM compile_cache
             WHERE artifact_kind = 'type_layout'
             ORDER BY created_at
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut artifact: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    artifact["metadata"]["size_bytes"] = JsonValue::from(size_bytes);
    let metadata_hash = hash_bytes(
        b"codedb/bytes/v1\0",
        canonical_json(&artifact["metadata"]).as_bytes(),
    );
    artifact["metadata_hash"] = JsonValue::from(metadata_hash.clone());
    conn.execute(
        "UPDATE compile_cache SET artifact_hash = ?1, artifact_json = ?2 WHERE cache_key = ?3",
        (metadata_hash, canonical_json(&artifact), cache_key),
    )
    .unwrap();
}

fn corrupt_first_type_layout_abi(db: &Path, pass: &str, return_: &str) {
    let conn = Connection::open(db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json
             FROM compile_cache
             WHERE artifact_kind = 'type_layout'
             ORDER BY created_at
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut artifact: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    artifact["metadata"]["abi"]["pass"] = JsonValue::from(pass);
    artifact["metadata"]["abi"]["return"] = JsonValue::from(return_);
    let metadata_hash = hash_bytes(
        b"codedb/bytes/v1\0",
        canonical_json(&artifact["metadata"]).as_bytes(),
    );
    artifact["metadata_hash"] = JsonValue::from(metadata_hash.clone());
    conn.execute(
        "UPDATE compile_cache SET artifact_hash = ?1, artifact_json = ?2 WHERE cache_key = ?3",
        (metadata_hash, canonical_json(&artifact), cache_key),
    )
    .unwrap();
}

fn hash_bytes(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
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
