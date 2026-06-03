use std::collections::BTreeSet;
use std::path::Path;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
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

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn parse_line_value<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .unwrap_or_else(|| panic!("missing line prefix {key:?} in:\n{text}"))
        .trim()
}

fn setup_shop(db: &Path) {
    run(&["init", path(db)]);
    run(&["import", path(db), "examples/shop.cdb"]);
}

fn object_cache_metadata_by_definition(db: &Path, definition_hash: &str) -> JsonValue {
    let conn = Connection::open(db).unwrap();
    let artifact_json: String = conn
        .query_row(
            "SELECT artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file' AND input_hash = ?1
             ORDER BY cache_key LIMIT 1",
            [definition_hash],
            |row| row.get(0),
        )
        .unwrap();
    parse_json(&artifact_json)["metadata"].clone()
}

#[test]
fn native_debug_metadata_maps_lowered_ops_to_text_ranges_and_survives_rename() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-debug.sqlite");
    let ir_file = temp.path().join("total.ir.json");
    let object_file = temp.path().join("total.o");
    let link_plan_file = temp.path().join("main.link.json");

    setup_shop(&db);
    let show_total = run(&["show", path(&db), "total"]);
    let total_symbol = parse_line_value(&show_total, "symbol ").to_string();
    let total_definition = parse_line_value(&show_total, "definition ").to_string();

    run(&["emit-ir", path(&db), "total", "--out", path(&ir_file)]);
    let ir_inspection = parse_json(&std::fs::read_to_string(&ir_file).unwrap());
    let ir = &ir_inspection["ir"];
    assert_eq!(ir["debug_map"]["schema"], "codedb/lowered-debug-map/v1");
    let lowered_debug_ops = ir["debug_map"]["operations"].as_array().unwrap();
    assert!(!lowered_debug_ops.is_empty());
    let op_ids = lowered_debug_ops
        .iter()
        .map(|op| op["lowered_op_id"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    let indexed_op_ids = ir["debug_map"]["expr_to_ops"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|entry| entry["lowered_op_ids"].as_array().unwrap())
        .map(|op_id| op_id.as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(indexed_op_ids, op_ids);
    for op in lowered_debug_ops {
        let value_id = op["value_id"].as_str().unwrap();
        assert_eq!(op["lowered_op_id"], format!("op:{value_id}"));
        assert!(op["expr_hash"].as_str().unwrap().starts_with("sha256:"));
    }

    run(&[
        "emit-object",
        path(&db),
        "total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&object_file),
    ]);
    let object_metadata = object_cache_metadata_by_definition(&db, &total_definition);
    assert_eq!(
        object_metadata["debug_metadata"]["schema"],
        "codedb/native-debug-metadata/v1"
    );
    assert_eq!(object_metadata["debug_metadata"]["text_section"], ".text");
    let text_size = object_metadata["debug_metadata"]["text_size"]
        .as_u64()
        .unwrap();
    let ranges = object_metadata["debug_metadata"]["ranges"]
        .as_array()
        .unwrap();
    assert_eq!(ranges.len(), lowered_debug_ops.len());
    for range in ranges {
        assert_eq!(range["symbol_hash"], total_symbol);
        assert_eq!(range["function_def_hash"], total_definition);
        assert!(range["expr_hash"].as_str().unwrap().starts_with("sha256:"));
        assert!(
            range["text_offset_start"].as_u64().unwrap()
                < range["text_offset_end"].as_u64().unwrap()
        );
        assert!(range["text_offset_end"].as_u64().unwrap() <= text_size);
    }

    run(&[
        "link-native",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&link_plan_file),
    ]);
    let link_plan = parse_json(&std::fs::read_to_string(&link_plan_file).unwrap());
    let linked_total = link_plan["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["symbol_hash"] == total_symbol)
        .expect("total object in link plan");
    assert_eq!(
        linked_total["debug_metadata"],
        object_metadata["debug_metadata"]
    );

    run(&["rename", path(&db), "total", "grand_total"]);
    run(&["emit-ir", path(&db), "grand_total", "--out", path(&ir_file)]);
    let renamed_ir = parse_json(&std::fs::read_to_string(&ir_file).unwrap());
    assert_eq!(
        renamed_ir["lowered_ir_hash"],
        ir_inspection["lowered_ir_hash"]
    );
    assert_eq!(renamed_ir["ir"]["debug_map"], ir["debug_map"]);

    run(&[
        "emit-object",
        path(&db),
        "grand_total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&object_file),
    ]);
    let renamed_object_metadata = object_cache_metadata_by_definition(&db, &total_definition);
    assert_eq!(
        renamed_object_metadata["debug_metadata"],
        object_metadata["debug_metadata"]
    );
    run(&["verify", path(&db)]);
}

#[test]
fn verify_rejects_malformed_native_debug_ranges() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-native-debug.sqlite");
    let object_file = temp.path().join("total.o");
    setup_shop(&db);
    run(&[
        "emit-object",
        path(&db),
        "total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&object_file),
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
    let mut value = parse_json(&artifact_json);
    let first_range = &mut value["metadata"]["debug_metadata"]["ranges"][0];
    first_range["text_offset_end"] = first_range["text_offset_start"].clone();
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (test_canonical_json(&value), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", path(&db)]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("malformed debug text range"));
}

#[test]
fn verify_rejects_link_plan_debug_metadata_that_disagrees_with_objects() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-link-debug.sqlite");
    let link_plan_file = temp.path().join("main.link.json");
    setup_shop(&db);
    run(&[
        "link-native",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&link_plan_file),
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
    let mut value = parse_json(&artifact_json);
    value["metadata"]["objects"][0]["debug_metadata"]["ranges"] = json!([]);
    let metadata_hash = test_metadata_hash(&value["metadata"]);
    value["metadata_hash"] = JsonValue::String(metadata_hash.clone());
    conn.execute(
        "UPDATE compile_cache
         SET artifact_json = ?1, artifact_hash = ?2
         WHERE cache_key = ?3",
        (test_canonical_json(&value), metadata_hash, cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", path(&db)]);
    assert!(stderr.contains("bad_link_plan"));
    assert!(stderr.contains("debug metadata does not match object artifact metadata"));
}

fn test_metadata_hash(value: &JsonValue) -> String {
    test_hash(b"codedb/bytes/v1\0", test_canonical_json(value).as_bytes())
}

fn test_hash(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
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
