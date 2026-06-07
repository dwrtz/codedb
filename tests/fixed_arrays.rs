use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
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
fn fixed_arrays_typecheck_lower_verify_export_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("fixed-arrays.sqlite");
    let source = temp.path().join("fixed-arrays.cdb");
    let projection = temp.path().join("fixed-arrays.projection.cdb");
    let rebuilt = temp.path().join("fixed-arrays-rebuilt.sqlite");
    let numbers_sum_ir = temp.path().join("numbers-sum.ir.json");
    let make_numbers_ir = temp.path().join("make-numbers.ir.json");
    let object_path = temp.path().join("make-lines.o");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn make_numbers() -> array<i64, 4> = [2, 4, 6, 8]

fn numbers_sum(values: array<i64, 4>, idx: i64) -> i64 =
  values[0] + values[1] + values[idx] + values[3]

fn numbers_sum_main() -> i64 = numbers_sum(make_numbers(), 2)

fn mutate_numbers() -> i64 effects[state] =
  let values: array<i64, 4> = make_numbers() in
  let _: unit = values[2] = 40 in
  values[2] + values[3]

fn make_lines() -> array<Line, 4> =
  [
    { price_cents: 10, qty: 1 },
    { price_cents: 20, qty: 2 },
    { price_cents: 30, qty: 3 },
    { price_cents: 5, qty: 6 }
  ]

fn line_total(lines: array<Line, 4>, idx: i64) -> i64 =
  lines[idx].price_cents * lines[idx].qty

fn line_total_main() -> i64 = line_total(make_lines(), 3)

fn mutate_line() -> i64 effects[state] =
  let lines: array<Line, 4> = make_lines() in
  let _: unit = lines[1].qty = 9 in
  lines[1].price_cents * lines[1].qty

fn main() -> i64 effects[state] =
  numbers_sum_main() + mutate_numbers() + line_total_main() + mutate_line()
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "numbers_sum_main"]).trim(), "20");
    assert_eq!(run(&["eval", path(&db), "mutate_numbers"]).trim(), "48");
    assert_eq!(run(&["eval", path(&db), "line_total_main"]).trim(), "30");
    assert_eq!(run(&["eval", path(&db), "mutate_line"]).trim(), "180");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "278");
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
    assert!(exported.contains("array<i64, 4>"));
    assert!(exported.contains("array<Line, 4>"));
    assert!(exported.contains("values[idx]"));
    assert!(exported.contains("lines[1].qty = 9"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "278");
    run(&["verify", path(&rebuilt)]);

    run(&[
        "emit-ir",
        path(&db),
        "numbers_sum",
        "--out",
        path(&numbers_sum_ir),
    ]);
    run(&[
        "emit-ir",
        path(&db),
        "make_numbers",
        "--out",
        path(&make_numbers_ir),
    ]);
    let numbers_sum_ir = read_json(&numbers_sum_ir);
    let make_numbers_ir = read_json(&make_numbers_ir);
    let op_names = op_names(&numbers_sum_ir);
    assert!(op_names.contains(&"addr_of_index".to_string()));
    assert!(op_names.contains(&"bounds_check".to_string()));
    assert_eq!(
        numbers_sum_ir["ir"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|op| op["op"] == "bounds_check" && op["len"] == 4)
            .count(),
        1
    );
    assert!(
        numbers_sum_ir["ir"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["op"] == "addr_of_index"
                && op["place"]["kind"] == "index"
                && op["place"].get("element_type_hash").is_some())
    );
    let debug_kinds = make_debug_kinds(&numbers_sum_ir);
    assert!(debug_kinds.contains(&"addr_of_index".to_string()));
    assert!(debug_kinds.contains(&"bounds_check".to_string()));

    let return_layout = make_numbers_ir["ir"]["type_layouts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|layout| layout["type_hash"] == make_numbers_ir["ir"]["return_type_hash"])
        .unwrap();
    assert_eq!(return_layout["kind"], "fixed_array");
    assert_eq!(return_layout["abi"]["pass"], "by_indirect");
    assert_eq!(return_layout["abi"]["return"], "hidden_return_slot");

    run(&[
        "emit-object",
        path(&db),
        "make_lines",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    assert_native_object_magic(&std::fs::read(&object_path).unwrap());

    if can_build_default_native_target() {
        run(&[
            "create-test",
            path(&db),
            "fixed_arrays_native",
            "--entry",
            "main",
            "--expect-i64",
            "278",
            "--native-required",
            "--json",
        ]);
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "278"})
        );
    }
}

#[test]
fn static_array_index_out_of_bounds_is_rejected_at_import() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("fixed-array-static-oob.sqlite");
    let source = temp.path().join("fixed-array-static-oob.cdb");

    std::fs::write(
        &source,
        r#"
fn bad() -> i64 =
  let values: array<i64, 2> = [1, 2] in
  values[2]
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("out of bounds"));
}

#[test]
fn dynamic_array_index_out_of_bounds_traps_natively_with_semantic_location() {
    // Phase 11 acceptance ("bounds trap maps to semantic expr_hash") for fixed
    // arrays at runtime. A non-literal (parameter) index cannot be rejected at
    // import, so it lowers to a real bounds check; out of range it must trap
    // natively, and the native trap must carry the indexing expression's
    // expr_hash (the slice analogue is covered in tests/slices.rs).
    let temp = tempdir().unwrap();
    let db = temp.path().join("array-dyn-oob.sqlite");
    let source = temp.path().join("array-dyn-oob.cdb");
    let at_ir_path = temp.path().join("at.ir.json");
    let exe_path = temp.path().join("array-dyn-oob");

    std::fs::write(
        &source,
        r#"
fn at(values: array<i64, 2>, i: i64) -> i64 = values[i]

fn main() -> i64 =
  let values: array<i64, 2> = [1, 2] in
  at(values, 2)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "error");
    assert_eq!(trace["diagnostics"][0]["kind"], "trap");

    run(&["emit-ir", path(&db), "at", "--out", path(&at_ir_path)]);
    let at_ir = read_json(&at_ir_path);
    let operations = at_ir["ir"]["operations"].as_array().unwrap();
    let bounds = operations
        .iter()
        .find(|op| op["op"] == "bounds_check")
        .expect("array bounds check");
    // Fixed-array length is a compile-time constant, so the check carries a
    // static `len` (unlike a slice, whose length is dynamic `len_value`).
    assert_eq!(bounds["len"], 2);

    let debug_ops = at_ir["ir"]["debug_map"]["operations"].as_array().unwrap();
    let bounds_debug = debug_ops
        .iter()
        .find(|op| op["value_id"] == bounds["id"])
        .expect("bounds debug op");
    let index_debug = debug_ops
        .iter()
        .find(|op| {
            op["lowered_op_kind"] == "addr_of_index" && op["expr_hash"] == bounds_debug["expr_hash"]
        })
        .expect("index debug op");
    assert_eq!(bounds_debug["expr_hash"], index_debug["expr_hash"]);
    assert_eq!(
        trace["diagnostics"][0]["location"]["expr_hash"],
        bounds_debug["expr_hash"]
    );

    if can_build_default_native_target() {
        run(&["build", path(&db), "main", "--out", path(&exe_path)]);
        let status = StdCommand::new(&exe_path)
            .status()
            .expect("run native array oob");
        assert!(!status.success());

        run(&[
            "create-test",
            path(&db),
            "array_oob_native_trap",
            "--entry",
            "main",
            "--expect-i64",
            "0",
            "--native-required",
            "--json",
        ]);
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["tests"][0]["native"]["status"], "failed");
        let native_location = report["tests"][0]["native"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|diagnostic| diagnostic["kind"] == "native_trap_semantic_location")
            .expect("native semantic trap diagnostic")["details"]["location"]
            .clone();
        assert_eq!(native_location["expr_hash"], bounds_debug["expr_hash"]);
    }
}

fn op_names(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect()
}

fn make_debug_kinds(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["debug_map"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["lowered_op_kind"].as_str().unwrap().to_string())
        .collect()
}

fn assert_native_object_magic(object_bytes: &[u8]) {
    if codedb::DEFAULT_NATIVE_TARGET == codedb::LINUX_X86_64_TARGET {
        assert_eq!(&object_bytes[..4], b"\x7fELF");
    } else {
        assert_eq!(&object_bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    }
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
