use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::{Value as JsonValue, json};
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

fn read_json(path: &Path) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn dynamic_vec_and_string_lower_verify_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("dynamic-buffers.sqlite");
    let source = temp.path().join("dynamic-buffers.cdb");
    let vec_ir_path = temp.path().join("vec-total.ir.json");
    let string_ir_path = temp.path().join("string-len.ir.json");
    let vec_layout_path = temp.path().join("vec.layout.json");
    let string_layout_path = temp.path().join("string.layout.json");
    let object_path = temp.path().join("vec-total.o");

    std::fs::write(
        &source,
        r#"
fn vec_total() -> i64 effects[alloc, state] =
  let xs: vec<i64> = vec_new(3) in
  let pushed1: unit = vec_push(xs, 10) in
  let pushed2: unit = vec_push(xs, 32) in
  vec_get(xs, 0) + vec_get(xs, 1) + vec_len(xs)

fn owned_string_len() -> i64 effects[alloc] =
  let s: string = string_new("hello") in
  string_len(s)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "vec_total"]).trim(), "44");
    assert_eq!(run(&["eval", path(&db), "owned_string_len"]).trim(), "5");
    run(&["verify", path(&db)]);

    run(&[
        "emit-type-layout",
        path(&db),
        "vec<i64>",
        "--out",
        path(&vec_layout_path),
    ]);
    let vec_layout = read_json(&vec_layout_path);
    assert_eq!(vec_layout["kind"], "vec");
    assert_eq!(vec_layout["copy_kind"], "move_only");
    assert_eq!(vec_layout["drop_kind"], "needs_drop");
    assert_eq!(vec_layout["contains_box"], false);
    assert_eq!(vec_layout["contains_owned_resource"], true);
    assert_eq!(vec_layout["abi"]["pass"], "by_indirect");

    run(&[
        "emit-type-layout",
        path(&db),
        "string",
        "--out",
        path(&string_layout_path),
    ]);
    let string_layout = read_json(&string_layout_path);
    assert_eq!(string_layout["kind"], "string");
    assert_eq!(string_layout["copy_kind"], "move_only");
    assert_eq!(string_layout["drop_kind"], "needs_drop");
    assert_eq!(string_layout["contains_box"], false);
    assert_eq!(string_layout["contains_owned_resource"], true);
    assert_eq!(string_layout["abi"]["pass"], "by_indirect");

    run(&[
        "emit-ir",
        path(&db),
        "vec_total",
        "--out",
        path(&vec_ir_path),
    ]);
    let vec_ir = read_json(&vec_ir_path);
    let vec_ops = op_names(&vec_ir);
    assert!(vec_ops.contains(&"vec_new".to_string()));
    assert!(vec_ops.contains(&"vec_push".to_string()));
    assert!(vec_ops.contains(&"vec_get".to_string()));
    assert!(vec_ops.contains(&"vec_len".to_string()));
    assert!(vec_ops.contains(&"drop".to_string()));

    run(&[
        "emit-ir",
        path(&db),
        "owned_string_len",
        "--out",
        path(&string_ir_path),
    ]);
    let string_ir = read_json(&string_ir_path);
    let string_ops = op_names(&string_ir);
    assert!(string_ops.contains(&"string_new".to_string()));
    assert!(string_ops.contains(&"string_len".to_string()));
    assert!(string_ops.contains(&"drop".to_string()));

    run(&[
        "emit-object",
        path(&db),
        "vec_total",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    assert_native_object_magic(&std::fs::read(&object_path).unwrap());

    if can_build_default_native_target() {
        let vec_test = parse_json(&run(&[
            "create-test",
            path(&db),
            "dynamic_vec_native",
            "--entry",
            "vec_total",
            "--expect-i64",
            "44",
            "--native-required",
            "--json",
        ]));
        assert_eq!(vec_test["status"], "applied");
        let string_test = parse_json(&run(&[
            "create-test",
            path(&db),
            "dynamic_string_native",
            "--entry",
            "owned_string_len",
            "--expect-i64",
            "5",
            "--native-required",
            "--json",
        ]));
        assert_eq!(string_test["status"], "applied");

        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        let mut actuals = report["tests"]
            .as_array()
            .expect("test reports")
            .iter()
            .map(|test| test["native"]["comparison"]["actual"].clone())
            .collect::<Vec<_>>();
        actuals.sort_by_key(|value| value["value"].as_str().unwrap_or_default().to_string());
        assert_eq!(
            actuals,
            vec![
                json!({"kind": "i64", "value": "44"}),
                json!({"kind": "i64", "value": "5"}),
            ]
        );
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

// A `vec<T>` / `string` buffer drops only its heap payload; it never iterates
// and drops individual elements. That is sound only because the element is
// restricted to Copy, non-reference, trivially-droppable 1/8-byte values
// (`require_phase20_buffer_element`). These cases lock in fail-closed rejection
// of every element category that would break the assumption (a leaked nested
// buffer/box, a dangling reference, or a too-wide element), so the boundary
// cannot silently regress.
#[test]
fn vec_rejects_unsupported_elements() {
    // (label, program, expected diagnostic substring)
    let cases = [
        (
            "nested-owned-buffer",
            "fn bad() -> i64 effects[alloc] =\n  let xs: vec<vec<i64>> = vec_new(1) in 0\n",
            "supports only Copy, non-reference elements with trivial drop",
        ),
        (
            "boxed-owned-element",
            "fn bad() -> i64 effects[alloc] =\n  let xs: vec<box<i64>> = vec_new(1) in 0\n",
            "supports only Copy, non-reference elements with trivial drop",
        ),
        (
            "reference-element",
            "fn bad<'a>(value: &'a i64) -> i64 effects[alloc] =\n  let xs: vec<&'a i64> = vec_new(1) in 0\n",
            "supports only Copy, non-reference elements with trivial drop",
        ),
        (
            "oversized-element",
            "record Wide { a: i64, b: i64 }\n\nfn bad() -> i64 effects[alloc] =\n  let xs: vec<Wide> = vec_new(1) in 0\n",
            "supports element sizes 1 and 8 bytes",
        ),
    ];

    for (label, program, expected) in cases {
        let temp = tempdir().unwrap();
        let db = temp.path().join(format!("reject-{label}.sqlite"));
        let source = temp.path().join(format!("reject-{label}.cdb"));
        std::fs::write(&source, program).unwrap();
        run(&["init", path(&db)]);
        let stderr = run_failure(&["import", path(&db), path(&source)]);
        assert!(
            stderr.contains(expected),
            "case {label}: expected {expected:?} in stderr, got: {stderr}"
        );
    }
}

// A `vec<T>` element may be any Copy, trivially-droppable 1- or 8-byte value,
// including a small record (the plan's "1- and 8-byte elements"). Only scalar
// elements are exercised above, so this locks in the small-record path: an
// 8-byte record round-trips through vec_push/vec_get natively and the buffer
// drop frees the payload exactly once (the record element needs no drop).
#[test]
fn vec_of_small_record_lowers_and_runs_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("vec-record.sqlite");
    let source = temp.path().join("vec-record.cdb");
    let object_path = temp.path().join("vec-record.o");

    std::fs::write(
        &source,
        r#"
record Pair { lo: i64 }

fn vec_record_total() -> i64 effects[alloc, state] =
  let xs: vec<Pair> = vec_new(2) in
  let pushed1: unit = vec_push(xs, {lo: 10}) in
  let pushed2: unit = vec_push(xs, {lo: 32}) in
  let got: Pair = vec_get(xs, 1) in
  got.lo + vec_len(xs)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "vec_record_total"]).trim(), "34");
    run(&["verify", path(&db)]);

    let layout_path = temp.path().join("vec-record.layout.json");
    run(&[
        "emit-type-layout",
        path(&db),
        "vec<Pair>",
        "--out",
        path(&layout_path),
    ]);
    let layout = read_json(&layout_path);
    assert_eq!(layout["kind"], "vec");
    assert_eq!(layout["copy_kind"], "move_only");
    assert_eq!(layout["drop_kind"], "needs_drop");
    // The element (Pair) carries no owned resource, so the buffer payload is the
    // only thing the drop frees.
    assert_eq!(layout["contains_owned_resource"], true);

    run(&[
        "emit-object",
        path(&db),
        "vec_record_total",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    assert_native_object_magic(&std::fs::read(&object_path).unwrap());

    if can_build_default_native_target() {
        let record_test = parse_json(&run(&[
            "create-test",
            path(&db),
            "vec_record_native",
            "--entry",
            "vec_record_total",
            "--expect-i64",
            "34",
            "--native-required",
            "--json",
        ]));
        assert_eq!(record_test["status"], "applied");

        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
    }
}
