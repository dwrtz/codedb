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
fn shared_slices_trace_round_trip_lower_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("slices.sqlite");
    let source = temp.path().join("slices.cdb");
    let projection = temp.path().join("slices.projection.cdb");
    let rebuilt = temp.path().join("slices-rebuilt.sqlite");
    let main_ir_path = temp.path().join("main.ir.json");
    let line_amount_ir_path = temp.path().join("line-amount.ir.json");
    let line_slice_layout_path = temp.path().join("line-slice-layout.json");
    let object_path = temp.path().join("main.o");

    std::fs::write(
        &source,
        r#"
record Line {
  amount: i64
}

record LineSlice<'a> {
  lines: slice<'a, Line>
}

fn first<'a>(s: slice<'a, i64>) -> i64 = s[0]

fn head_plus_tail<'a>(s: slice<'a, i64>) -> i64 =
  let tail: slice<'a, i64> = subslice(s, 1, 3) in
  len(s) + first(s) + tail[1]

fn line_amount<'a>(lines: slice<'a, Line>) -> i64 = lines[0].amount

fn main<'a>() -> i64 =
  let values: array<i64, 4> = [10, 20, 30, 40] in
  let s: slice<'a, i64> = slice(values) in
  head_plus_tail(s)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "44");

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "44"}));
    let trace_kinds = trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "eval_expr")
        .filter_map(|event| event["expr_kind"].as_str())
        .collect::<Vec<_>>();
    assert!(trace_kinds.contains(&"slice_from_array"));
    assert!(trace_kinds.contains(&"subslice"));
    assert!(trace_kinds.contains(&"slice_len"));
    assert!(trace_kinds.contains(&"array_index"));
    run(&["verify", path(&db)]);
    run(&[
        "emit-type-layout",
        path(&db),
        "LineSlice",
        "--out",
        path(&line_slice_layout_path),
    ]);

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("slice<'a, i64>"));
    assert!(exported.contains("slice<'a, Line>"));
    assert!(exported.contains("slice(values)"));
    assert!(exported.contains("subslice(s, 1, 3)"));
    assert!(exported.contains("tail[1]"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "44");
    run(&["verify", path(&rebuilt)]);

    run(&["emit-ir", path(&db), "main", "--out", path(&main_ir_path)]);
    run(&[
        "emit-ir",
        path(&db),
        "line_amount",
        "--out",
        path(&line_amount_ir_path),
    ]);
    let main_ir = read_json(&main_ir_path);
    let main_ops = op_names(&main_ir);
    assert!(main_ops.contains(&"construct_slice".to_string()));
    let debug_kinds = debug_kinds(&main_ir);
    assert!(debug_kinds.contains(&"construct_slice".to_string()));

    let line_amount_ir = read_json(&line_amount_ir_path);
    let line_layout = line_amount_ir["ir"]["type_layouts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|layout| layout["kind"] == "slice")
        .expect("slice layout");
    assert_eq!(line_layout["size_bytes"], 16);
    assert_eq!(line_layout["align_bytes"], 8);
    assert_eq!(line_layout["abi"]["pass"], "by_indirect");
    assert_eq!(line_layout["abi"]["return"], "hidden_return_slot");

    let line_slice_layout = read_json(&line_slice_layout_path);
    assert_eq!(line_slice_layout["kind"], "record");
    assert_eq!(line_slice_layout["size_bytes"], 16);
    assert_eq!(line_slice_layout["fields"][0]["size_bytes"], 16);
    assert_eq!(line_slice_layout["contains_reference"], true);
    assert_eq!(line_slice_layout["contains_mut_reference"], false);
    assert_eq!(line_slice_layout["copy_kind"], "copy");

    run(&[
        "emit-object",
        path(&db),
        "main",
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
            "slices_native",
            "--entry",
            "main",
            "--expect-i64",
            "44",
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
            json!({"kind": "i64", "value": "44"})
        );
    }
}

#[test]
fn mut_slice_store_runs_native_and_requires_state_effect() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("mut-slices.sqlite");
    let source = temp.path().join("mut-slices.cdb");
    let bad_db = temp.path().join("mut-slices-bad.sqlite");
    let bad_source = temp.path().join("mut-slices-bad.cdb");
    let write_ir_path = temp.path().join("write-second.ir.json");
    let mut_slice_layout_path = temp.path().join("mut-slice-holder-layout.json");

    std::fs::write(
        &source,
        r#"
record MutSliceHolder<'a> {
  values: mut_slice<'a, i64>
}

fn write_second<'a>(s: mut_slice<'a, i64>) -> i64 effects[state] =
  let _: unit = s[1] = 99 in
  s[1]

fn main<'a>() -> i64 effects[state] =
  let values: array<i64, 3> = [1, 2, 3] in
  let s: mut_slice<'a, i64> = mut_slice(values) in
  write_second(s)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "99");
    run(&["verify", path(&db)]);
    run(&[
        "emit-type-layout",
        path(&db),
        "MutSliceHolder",
        "--out",
        path(&mut_slice_layout_path),
    ]);

    run(&[
        "emit-ir",
        path(&db),
        "write_second",
        "--out",
        path(&write_ir_path),
    ]);
    let write_ir = read_json(&write_ir_path);
    let ops = op_names(&write_ir);
    assert!(ops.contains(&"slice_data".to_string()));
    assert!(ops.contains(&"slice_len".to_string()));
    assert!(ops.contains(&"bounds_check".to_string()));
    assert!(ops.contains(&"store".to_string()));
    assert!(
        write_ir["ir"]["type_layouts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|layout| layout["kind"] == "slice"
                && layout["size_bytes"] == 16
                && layout["abi"]["pass"] == "by_indirect")
    );
    let mut_slice_layout = read_json(&mut_slice_layout_path);
    assert_eq!(mut_slice_layout["kind"], "record");
    assert_eq!(mut_slice_layout["fields"][0]["size_bytes"], 16);
    assert_eq!(mut_slice_layout["copy_kind"], "move_only");
    assert_eq!(mut_slice_layout["contains_reference"], true);
    assert_eq!(mut_slice_layout["contains_mut_reference"], true);

    if can_build_default_native_target() {
        run(&[
            "create-test",
            path(&db),
            "mut_slices_native",
            "--entry",
            "main",
            "--expect-i64",
            "99",
            "--native-required",
            "--json",
        ]);
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["entry_effects"], json!(["state"]));
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "99"})
        );
    }

    std::fs::write(
        &bad_source,
        r#"
fn bad<'a>() -> i64 =
  let values: array<i64, 2> = [1, 2] in
  let s: mut_slice<'a, i64> = mut_slice(values) in
  let _: unit = s[0] = 7 in
  s[0]
"#,
    )
    .unwrap();
    run(&["init", path(&bad_db)]);
    bin()
        .args(["import", path(&bad_db), path(&bad_source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_effects"))
        .stderr(predicate::str::contains("state"));
}

#[test]
fn slice_out_of_bounds_traps_and_keeps_semantic_location_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("slice-oob.sqlite");
    let source = temp.path().join("slice-oob.cdb");
    let main_ir_path = temp.path().join("main.ir.json");
    let exe_path = temp.path().join("slice-oob");

    std::fs::write(
        &source,
        r#"
fn main<'a>() -> i64 =
  let values: array<i64, 2> = [1, 2] in
  let s: slice<'a, i64> = slice(values) in
  s[2]
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "error");
    assert_eq!(trace["result"], JsonValue::Null);
    assert_eq!(trace["diagnostics"][0]["kind"], "trap");
    assert!(
        trace["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("slice index")
    );

    run(&["emit-ir", path(&db), "main", "--out", path(&main_ir_path)]);
    let main_ir = read_json(&main_ir_path);
    let operations = main_ir["ir"]["operations"].as_array().unwrap();
    let bounds = operations
        .iter()
        .find(|op| op["op"] == "bounds_check" && op.get("len_value").is_some())
        .expect("dynamic slice bounds check");
    assert_eq!(bounds["len"], 0);

    let debug_ops = main_ir["ir"]["debug_map"]["operations"].as_array().unwrap();
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
        let status = StdCommand::new(&exe_path).status().expect("run native oob");
        assert!(!status.success());
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

fn debug_kinds(ir: &JsonValue) -> Vec<String> {
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
