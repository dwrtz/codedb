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
    path.to_str().expect("utf8 path")
}

fn read_json(path: &Path) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn scalar_functions_lower_through_places_and_still_emit_native_objects() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("scalar-memory-ir.sqlite");
    let source = temp.path().join("scalar-memory-ir.cdb");
    let ir_path = temp.path().join("scalar.ir.json");
    let object_path = temp.path().join("scalar.o");

    std::fs::write(
        &source,
        r#"
fn scalar(x: i64) -> i64 = let y: i64 = x + 1 in y * 2
fn main() -> i64 = scalar(59)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");
    run(&["emit-ir", path(&db), "scalar", "--out", path(&ir_path)]);

    let ir = read_json(&ir_path);
    assert_eq!(ir["ir"]["schema"], "codedb/lowered-function-ir/v2");
    assert_eq!(ir["ir"]["locals"].as_array().unwrap().len(), 1);
    let op_names = op_names(&ir);
    assert!(op_names.contains(&"addr_of_param".to_string()));
    assert!(op_names.contains(&"load".to_string()));
    assert!(op_names.contains(&"addr_of_local".to_string()));
    assert!(op_names.contains(&"store".to_string()));
    assert!(!op_names.contains(&"param".to_string()));

    run(&[
        "emit-object",
        path(&db),
        "scalar",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&object_path),
    ]);
    let object_bytes = std::fs::read(&object_path).unwrap();
    assert_eq!(&object_bytes[..4], b"\x7fELF");
    run(&["verify", path(&db)]);
}

#[test]
fn record_field_access_lowers_to_place_address_and_load_scaffold() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("record-field-memory-ir.sqlite");
    let source = temp.path().join("record-field-memory-ir.cdb");
    let ir_path = temp.path().join("add-tax.ir.json");
    let object_path = temp.path().join("add-tax.o");

    std::fs::write(
        &source,
        r#"
fn add_tax(order: record { amount: i64, tax: i64 }) -> i64 = order.amount + order.tax
fn main() -> i64 = add_tax({ amount: 100, tax: 20 })
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");
    run(&["emit-ir", path(&db), "add_tax", "--out", path(&ir_path)]);
    run(&["verify", path(&db)]);

    let ir = read_json(&ir_path);
    let op_names = op_names(&ir);
    assert_eq!(
        op_names
            .iter()
            .filter(|op| op.as_str() == "addr_of_field")
            .count(),
        2
    );
    assert_eq!(
        op_names.iter().filter(|op| op.as_str() == "load").count(),
        2
    );
    let fields = ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|op| op["op"] == "addr_of_field")
        .map(|op| op["place"]["field"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(fields, vec!["amount".to_string(), "tax".to_string()]);
    let debug_kinds = ir["ir"]["debug_map"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["lowered_op_kind"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(debug_kinds.contains(&"addr_of_field".to_string()));
    assert!(debug_kinds.contains(&"load".to_string()));

    run(&[
        "emit-object",
        path(&db),
        "add_tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&object_path),
    ]);
    let object_bytes = std::fs::read(&object_path).unwrap();
    assert_eq!(&object_bytes[..4], b"\x7fELF");
}

#[test]
fn verify_rejects_lowered_load_with_incompatible_address_type() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-load-memory-ir.sqlite");
    let source = temp.path().join("bad-load-memory-ir.cdb");
    let ir_path = temp.path().join("id.ir.json");

    std::fs::write(
        &source,
        r#"
fn id(x: i64) -> i64 = x
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["emit-ir", path(&db), "id", "--out", path(&ir_path)]);
    corrupt_first_load_type(&db, type_hash_for("Bool"));

    bin()
        .args(["verify", path(&db)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_lowered_ir"))
        .stderr(predicate::str::contains("lowered load type mismatch"));
}

fn op_names(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect()
}

fn corrupt_first_load_type(db: &Path, type_hash: String) {
    let conn = Connection::open(db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json
             FROM compile_cache
             WHERE artifact_kind = 'lowered_ir'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    let operations = value["metadata"]["operations"].as_array_mut().unwrap();
    let load = operations
        .iter_mut()
        .find(|op| op["op"].as_str() == Some("load"))
        .expect("load operation");
    load["type_hash"] = JsonValue::String(type_hash);
    let metadata_hash = bytes_hash(canonical_json(&value["metadata"]).as_bytes());
    value["metadata_hash"] = JsonValue::String(metadata_hash.clone());
    conn.execute(
        "UPDATE compile_cache
         SET artifact_json = ?1, artifact_hash = ?2
         WHERE cache_key = ?3",
        (canonical_json(&value), metadata_hash, cache_key),
    )
    .unwrap();
}

fn type_hash_for(kind: &str) -> String {
    let payload = match kind {
        "I64" => r#"{"type_kind":"I64"}"#,
        "Bool" => r#"{"type_kind":"Bool"}"#,
        "Unit" => r#"{"type_kind":"Unit"}"#,
        other => panic!("unsupported builtin type {other}"),
    };
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/object/v1\0");
    hasher.update(b"Type");
    hasher.update([0]);
    hasher.update(b"1");
    hasher.update([0]);
    hasher.update(payload.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn bytes_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/bytes/v1\0");
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
