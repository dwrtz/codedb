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

/// Count every `op` of kind `op_name` in a lowered-IR JSON tree, descending into
/// `if`/`case` blocks (drops live inside arm/branch blocks and at scope exit).
fn count_ops_recursive(value: &JsonValue, op_name: &str) -> usize {
    let mut count = 0;
    match value {
        JsonValue::Object(map) => {
            if map.get("op").and_then(JsonValue::as_str) == Some(op_name) {
                count += 1;
            }
            for child in map.values() {
                count += count_ops_recursive(child, op_name);
            }
        }
        JsonValue::Array(items) => {
            for child in items {
                count += count_ops_recursive(child, op_name);
            }
        }
        _ => {}
    }
    count
}

/// Import `source`, assert the evaluator returns `expected`, verify, and (on a
/// native target) build + run the native binary and confirm it matches.
fn check_array_native(name: &str, source: &str, expected: i64) {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    assert_eq!(
        run(&["eval", path(&db), "main"]).trim(),
        expected.to_string(),
        "{name}: evaluator"
    );
    run(&["verify", path(&db)]);
    if can_build_default_native_target() {
        run(&[
            "create-test",
            path(&db),
            &format!("{name}_native"),
            "--entry",
            "main",
            "--expect-i64",
            &expected.to_string(),
            "--native-required",
            "--json",
        ]);
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed", "{name}: native status");
        assert_eq!(report["native_mismatches"], 0, "{name}: native mismatches");
    }
}

// ===========================================================================
// R14 #1 — constant-index array-element partial moves. Moving a move-only element
// `xs[N]` out of an array leaves the array partially moved; lowering drops the
// live remaining elements by index (element-granular drop glue) so every owned
// value is freed exactly once. A dynamic index stays fail-closed.
// ===========================================================================

#[test]
fn array_element_partial_move_drops_siblings_native() {
    // Move element 0 (a box) out and consume it; elements 1 and 2 must be dropped at
    // scope exit. Exactly 1 move + 2 drops; a double-free aborts the native run.
    let source = "fn f() -> i64 effects[alloc] =\n\
         let xs: array<box<i64>, 3> = [box_new(10), box_new(20), box_new(30)] in\n\
         unbox(xs[0])\n\
         fn main() -> i64 effects[alloc] = f()\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("ae.sqlite");
    let src = temp.path().join("ae.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);
    let ir_path = temp.path().join("f.ir.json");
    run(&["emit-ir", path(&db), "f", "--out", path(&ir_path)]);
    let ir = read_json(&ir_path);
    assert_eq!(count_ops_recursive(&ir, "move"), 1, "xs[0] is moved once");
    assert_eq!(
        count_ops_recursive(&ir, "drop"),
        2,
        "the two live sibling elements xs[1], xs[2] are each dropped once"
    );
    check_array_native("array_elem_move", source, 10);
}

#[test]
fn array_element_conditional_move_compensates_native() {
    // Element 0 is moved only on the `then` path; on `else` the merge compensation must
    // drop it, so every path frees all three boxes exactly once (1 move + 3 drops:
    // xs[0] compensated on `else`, xs[1]/xs[2] at scope exit). A miss leaks or aborts.
    let source = "fn f(flag: bool) -> i64 effects[alloc] =\n\
         let xs: array<box<i64>, 3> = [box_new(10), box_new(20), box_new(30)] in\n\
         if flag then unbox(xs[0]) else 0\n\
         fn main() -> i64 effects[alloc] = f(2 < 1) + f(1 < 2)\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("ac.sqlite");
    let src = temp.path().join("ac.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);
    let ir_path = temp.path().join("f.ir.json");
    run(&["emit-ir", path(&db), "f", "--out", path(&ir_path)]);
    let ir = read_json(&ir_path);
    assert_eq!(count_ops_recursive(&ir, "move"), 1, "xs[0] moved on the then path");
    assert_eq!(
        count_ops_recursive(&ir, "drop"),
        3,
        "xs[0] compensated on else + xs[1]/xs[2] at scope exit"
    );
    check_array_native("array_elem_cond", source, 10);
}

#[test]
fn array_two_element_moves_drops_remaining_native() {
    // Move elements 0 and 2; only element 1 survives to be dropped. 2 moves + 1 drop.
    let source = "fn f() -> i64 effects[alloc] =\n\
         let xs: array<box<i64>, 3> = [box_new(10), box_new(20), box_new(30)] in\n\
         unbox(xs[0]) + unbox(xs[2])\n\
         fn main() -> i64 effects[alloc] = f()\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("a2.sqlite");
    let src = temp.path().join("a2.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);
    let ir_path = temp.path().join("f.ir.json");
    run(&["emit-ir", path(&db), "f", "--out", path(&ir_path)]);
    let ir = read_json(&ir_path);
    assert_eq!(count_ops_recursive(&ir, "move"), 2, "xs[0] and xs[2] moved");
    assert_eq!(count_ops_recursive(&ir, "drop"), 1, "only xs[1] survives to be dropped");
    check_array_native("array_two_moves", source, 40);
}

#[test]
fn array_element_field_partial_move_native() {
    // A mixed path: move a record FIELD of an array element (`xs[0].b`). Element 1 is
    // dropped wholly; element 0's remaining fields (none) are skipped. 1 move + 1 drop.
    let source = "record Boxed { b: box<i64> }\n\
         fn f() -> i64 effects[alloc] =\n\
         let xs: array<Boxed, 2> = [{ b: box_new(10) }, { b: box_new(20) }] in\n\
         unbox(xs[0].b)\n\
         fn main() -> i64 effects[alloc] = f()\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("af.sqlite");
    let src = temp.path().join("af.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);
    let ir_path = temp.path().join("f.ir.json");
    run(&["emit-ir", path(&db), "f", "--out", path(&ir_path)]);
    let ir = read_json(&ir_path);
    assert_eq!(count_ops_recursive(&ir, "move"), 1, "xs[0].b moved");
    assert_eq!(count_ops_recursive(&ir, "drop"), 1, "xs[1] dropped wholly");
    check_array_native("array_elem_field", source, 10);
}

#[test]
fn dynamic_index_array_element_move_fails_closed() {
    // A move out of a DYNAMIC index cannot be element-granular-dropped (the scaffold
    // can't know which element survived), so it fails closed with a clean diagnostic.
    let temp = tempdir().unwrap();
    let db = temp.path().join("ad.sqlite");
    let src = temp.path().join("ad.cdb");
    std::fs::write(
        &src,
        "fn g(i: i64) -> i64 effects[alloc] =\n\
         let xs: array<box<i64>, 3> = [box_new(10), box_new(20), box_new(30)] in\n\
         unbox(xs[i])\n\
         fn main() -> i64 effects[alloc] = g(0)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let stderr = String::from_utf8(
        bin()
            .args(["import", path(&db), path(&src)])
            .assert()
            .failure()
            .get_output()
            .stderr
            .clone(),
    )
    .unwrap();
    assert!(
        stderr.contains("dynamic index") && stderr.contains("constant-index"),
        "expected a dynamic-index fail-closed diagnostic, got: {stderr}"
    );
}

#[test]
fn array_element_move_program_round_trips() {
    // The accepted program (with the element move) projects back to re-parseable
    // source and import -> export -> import is a root-hash fixpoint (SPEC_V3 §11).
    let source = "fn f() -> i64 effects[alloc] =\n\
         let xs: array<box<i64>, 3> = [box_new(10), box_new(20), box_new(30)] in\n\
         unbox(xs[0])\n\
         fn main() -> i64 effects[alloc] = f()\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("ar.sqlite");
    let src = temp.path().join("ar.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    let root1 = run(&["import", path(&db), path(&src)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("import prints root");
    let export = temp.path().join("ar.export.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(exported.contains("unbox(xs[0])"), "element move projects: {exported}");
    let db2 = temp.path().join("ar2.sqlite");
    run(&["init", path(&db2)]);
    let root2 = run(&["import", path(&db2), path(&export)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("re-import prints root");
    assert_eq!(root1, root2, "array element move import->export->import must be a fixpoint");
    run(&["verify", path(&db2)]);
}
