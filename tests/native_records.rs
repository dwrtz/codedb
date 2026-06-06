use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
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

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn read_json(path: &Path) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn records_compile_end_to_end_with_params_returns_references_and_native_tests() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-records.sqlite");
    let source = temp.path().join("native-records.cdb");
    let main_ir_path = temp.path().join("main.ir.json");
    let make_tiny_ir_path = temp.path().join("make-tiny.ir.json");
    let make_big_ir_path = temp.path().join("make-big.ir.json");
    let object_path = temp.path().join("make-big.o");

    std::fs::write(
        &source,
        r#"
record Pair {
  left: i64
  right: i64
}

record Tiny {
  only: i64
}

record Big {
  a: i64
  b: i64
  c: i64
  d: i64
}

record Line {
  price_cents: i64
  qty: i64
}

record LineView<'a> {
  line: &'a Line
}

fn sum_pair(pair: Pair) -> i64 = pair.left + pair.right

fn make_pair() -> Pair =
  let pair: Pair = { left: 10, right: 7 } in
  pair

fn sum_tiny(tiny: Tiny) -> i64 = tiny.only

fn make_tiny() -> Tiny =
  let tiny: Tiny = { only: 11 } in
  tiny

fn sum_big(big: Big) -> i64 = big.a + big.b + big.c + big.d

fn make_big() -> Big =
  let big: Big = { a: 1, b: 2, c: 3, d: 4 } in
  big

fn line_total<'a>(view: LineView<'a>) -> i64 =
  view.line.price_cents * view.line.qty

fn refs_main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let view: LineView<'a> = { line: &'a line } in
  line_total(view)

fn main() -> i64 = sum_pair({ left: 2, right: 3 }) + sum_pair(make_pair()) + sum_tiny({ only: 6 }) + sum_tiny(make_tiny()) + sum_big(make_big()) + refs_main()
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "149");

    run(&["emit-ir", path(&db), "main", "--out", path(&main_ir_path)]);
    run(&[
        "emit-ir",
        path(&db),
        "make_tiny",
        "--out",
        path(&make_tiny_ir_path),
    ]);
    run(&[
        "emit-ir",
        path(&db),
        "make_big",
        "--out",
        path(&make_big_ir_path),
    ]);
    let main_ir = read_json(&main_ir_path);
    let make_tiny_ir = read_json(&make_tiny_ir_path);
    let make_big_ir = read_json(&make_big_ir_path);
    assert!(
        main_ir["ir"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["op"] == "call" && op.get("return_address").is_some())
    );
    let tiny_return_layout = make_tiny_ir["ir"]["type_layouts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|layout| layout["type_hash"] == make_tiny_ir["ir"]["return_type_hash"])
        .unwrap();
    assert_eq!(tiny_return_layout["kind"], "record");
    assert_eq!(tiny_return_layout["abi"]["pass"], "by_value");
    assert_eq!(tiny_return_layout["abi"]["return"], "by_value");
    let big_return_layout = make_big_ir["ir"]["type_layouts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|layout| layout["type_hash"] == make_big_ir["ir"]["return_type_hash"])
        .unwrap();
    assert_eq!(big_return_layout["kind"], "record");
    assert_eq!(big_return_layout["abi"]["pass"], "by_indirect");
    assert_eq!(big_return_layout["abi"]["return"], "hidden_return_slot");

    run(&[
        "emit-object",
        path(&db),
        "make_big",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    let object_bytes = std::fs::read(&object_path).unwrap();
    if codedb::DEFAULT_NATIVE_TARGET == codedb::LINUX_X86_64_TARGET {
        assert_eq!(&object_bytes[..4], b"\x7fELF");
    } else {
        assert_eq!(&object_bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    }
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "native_records",
            "--entry",
            "main",
            "--expect-i64",
            "149",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");

        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["passed"], 1);
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "149"})
        );
    }
}

#[test]
fn record_return_test_values_serialize_and_compare_reference_results() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("record-test-values.sqlite");
    let source = temp.path().join("record-test-values.cdb");
    let apply = temp.path().join("record-test-values.apply.json");

    std::fs::write(
        &source,
        r#"
record Pair {
  left: i64
  right: i64
}

fn make_pair() -> Pair =
  let pair: Pair = { left: 10, right: 7 } in
  pair

fn main() -> i64 = 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    std::fs::write(
        &apply,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "operations": [
                {
                    "kind": "create_test",
                    "name": "make_pair_record",
                    "entry": "make_pair",
                    "native_required": true,
                    "expected": {
                        "kind": "record",
                        "fields": [
                            { "name": "left", "value": { "kind": "i64", "value": "10" } },
                            { "name": "right", "value": { "kind": "i64", "value": "7" } }
                        ]
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let created = parse_json(&run(&["apply", path(&db), "--json", path(&apply)]));
    assert_eq!(created["status"], "applied");

    let listed = parse_json(&run(&["test", path(&db), "--list", "--json"]));
    assert_eq!(listed["tests"][0]["expected"]["kind"], "record");
    assert_eq!(listed["tests"][0]["native_required"], true);
    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["passed"], 1);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    } else {
        assert_eq!(report["status"], "failed");
        assert_eq!(report["unsupported"], 1);
    }
    assert_eq!(report["tests"][0]["reference"]["status"], "passed");
    assert_eq!(
        report["tests"][0]["reference"]["actual"],
        json!({
            "kind": "record",
            "fields": [
                { "name": "left", "value": { "kind": "i64", "value": "10" } },
                { "name": "right", "value": { "kind": "i64", "value": "7" } }
            ]
        })
    );
    run(&["verify", path(&db)]);
}

#[test]
fn direct_record_literal_return_compiles_and_runs_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("direct-record-return.sqlite");
    let source = temp.path().join("direct-record-return.cdb");
    let apply = temp.path().join("direct-record-return.apply.json");

    std::fs::write(
        &source,
        r#"
record Pair {
  left: i64
  right: i64
}

fn make_pair() -> Pair = { left: 10, right: 7 }

fn main() -> i64 =
  let pair: Pair = make_pair() in
  pair.left + pair.right
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "17");
    std::fs::write(
        &apply,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "operations": [
                {
                    "kind": "create_test",
                    "name": "direct_record_return",
                    "entry": "make_pair",
                    "native_required": true,
                    "expected": {
                        "kind": "record",
                        "fields": [
                            { "name": "left", "value": { "kind": "i64", "value": "10" } },
                            { "name": "right", "value": { "kind": "i64", "value": "7" } }
                        ]
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let created = parse_json(&run(&["apply", path(&db), "--json", path(&apply)]));
    assert_eq!(created["status"], "applied");

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    } else {
        assert_eq!(report["status"], "failed");
        assert_eq!(report["unsupported"], 1);
    }
    run(&["verify", path(&db)]);
}

#[test]
fn by_value_record_return_test_uses_layout_abi() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("tiny-record-test.sqlite");
    let source = temp.path().join("tiny-record.cdb");
    let apply = temp.path().join("tiny-record.apply.json");

    std::fs::write(
        &source,
        r#"
record Tiny {
  only: i64
}

fn make_tiny() -> Tiny =
  let tiny: Tiny = { only: 11 } in
  tiny

fn main() -> i64 = 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    std::fs::write(
        &apply,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "operations": [
                {
                    "kind": "create_test",
                    "name": "make_tiny_record",
                    "entry": "make_tiny",
                    "native_required": true,
                    "expected": {
                        "kind": "record",
                        "fields": [
                            { "name": "only", "value": { "kind": "i64", "value": "11" } }
                        ]
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let created = parse_json(&run(&["apply", path(&db), "--json", path(&apply)]));
    assert_eq!(created["status"], "applied");

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["passed"], 1);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    } else {
        assert_eq!(report["status"], "failed");
        assert_eq!(report["unsupported"], 1);
    }
    run(&["verify", path(&db)]);
}

#[test]
fn compact_record_sizes_compile_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("compact-record.sqlite");
    let source = temp.path().join("compact-record.cdb");
    let object_path = temp.path().join("make-flags.o");

    std::fs::write(
        &source,
        r#"
record Flags {
  a: bool
  b: bool
}

fn first(flags: Flags) -> bool = flags.a

fn make_flags() -> Flags =
  let flags: Flags = { a: true, b: false } in
  flags

fn main() -> bool = first(make_flags())
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "true");
    run(&[
        "emit-object",
        path(&db),
        "make_flags",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "compact_record_native",
            "--entry",
            "main",
            "--expect-bool",
            "true",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    }
}

#[test]
fn region_parameterized_record_return_lowers_and_runs_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("region-record-return.sqlite");
    let source = temp.path().join("region-record-return.cdb");
    let main_ir_path = temp.path().join("main.ir.json");
    let id_view_ir_path = temp.path().join("id-view.ir.json");
    let id_view_object_path = temp.path().join("id-view.o");

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

fn id_view<'a>(view: LineView<'a>) -> LineView<'a> = view

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let view: LineView<'a> = { line: &'a line } in
  let same: LineView<'a> = id_view(view) in
  same.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "25");
    run(&["emit-ir", path(&db), "main", "--out", path(&main_ir_path)]);
    run(&[
        "emit-ir",
        path(&db),
        "id_view",
        "--out",
        path(&id_view_ir_path),
    ]);
    let id_view_ir = read_json(&id_view_ir_path);
    let id_view_return_layout = id_view_ir["ir"]["type_layouts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|layout| layout["type_hash"] == id_view_ir["ir"]["return_type_hash"])
        .unwrap();
    assert_eq!(id_view_return_layout["kind"], "record");
    assert_eq!(id_view_return_layout["size_bytes"], 8);
    assert_eq!(id_view_return_layout["abi"]["return"], "by_value");
    run(&[
        "emit-object",
        path(&db),
        "id_view",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&id_view_object_path),
    ]);
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "region_record_return_native",
            "--entry",
            "main",
            "--expect-i64",
            "25",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    }
}

#[test]
fn native_object_cache_key_changes_when_named_record_layout_changes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("record-layout-object-cache.sqlite");
    let source = temp.path().join("record-layout-object-cache.cdb");
    let add_field = temp.path().join("add-field.json");
    let object_path = temp.path().join("first.o");

    std::fs::write(
        &source,
        r#"
record Pair {
  left: i64
}

fn first(pair: Pair) -> i64 = pair.left
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "emit-object",
        path(&db),
        "first",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    let first_keys = object_cache_key_payloads(&db);
    assert_eq!(first_keys.len(), 1);
    let first_cache_key = first_keys[0].0.clone();
    let first_dependencies = key_dependency_implementations(&first_keys[0].1);
    assert!(!first_dependencies.is_empty());

    std::fs::write(
        &add_field,
        r#"{ "kind": "add_field", "type": "Pair", "field": { "name": "right", "type": "i64" } }"#,
    )
    .unwrap();
    run(&["apply", path(&db), "--json", path(&add_field)]);
    run(&[
        "emit-object",
        path(&db),
        "first",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);

    let object_keys = object_cache_key_payloads(&db);
    assert_eq!(object_keys.len(), 2);
    assert!(object_keys.iter().any(|(key, _)| key == &first_cache_key));
    let second_key = object_keys
        .iter()
        .find(|(key, _)| key != &first_cache_key)
        .expect("new object cache key after layout change");
    let second_dependencies = key_dependency_implementations(&second_key.1);
    assert!(!second_dependencies.is_empty());
    assert_ne!(first_dependencies, second_dependencies);
    run(&["verify", path(&db)]);
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

fn object_cache_key_payloads(db: &Path) -> Vec<(String, JsonValue)> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT cache_key, cache_key_json
             FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key",
        )
        .unwrap();
    stmt.query_map([], |row| {
        let cache_key: String = row.get(0)?;
        let cache_key_json: String = row.get(1)?;
        let payload: JsonValue = serde_json::from_str(&cache_key_json).unwrap();
        Ok((cache_key, payload))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn key_dependency_implementations(key_payload: &JsonValue) -> Vec<String> {
    key_payload["dependency_implementation_hashes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect()
}
