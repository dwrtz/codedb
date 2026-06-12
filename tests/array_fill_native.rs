// Phase 13 (R9): `[value; count]` array repeat/fill initializer over the fixed-array
// model. `value` is evaluated ONCE and replicated into all `count` Copy slots; the
// oracle is eval == native. `[0; 1024]` must type-check and lower (its 8 KB frame
// exceeds the v0 backend limit, so it is gated at "lowers", per the plan). The fill
// form round-trips through the `.cdb` projection as `[value; count]`.
use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value as JsonValue;
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

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

const SOURCE: &str = r#"
record Point { x: i64, y: i64 }

fn fill_sum() -> i64 =
  let xs: array<i64, 4> = [7; 4] in
  xs[0] + xs[1] + xs[2] + xs[3]

fn fill_byte() -> i64 =
  let xs: array<u8, 8> = [3; 8] in
  to_i64(xs[0]) + to_i64(xs[7])

fn fill_record() -> i64 =
  let ps: array<Point, 3> = [{x: 2, y: 5}; 3] in
  ps[0].x + ps[1].y + ps[2].x

fn fill_one() -> i64 =
  let xs: array<i64, 1> = [42; 1] in
  xs[0]

fn big_len() -> i64 =
  let xs: array<i64, 1024> = [0; 1024] in
  xs[0] + xs[1023]
"#;

/// (entry, expected i64) — the natively runnable, in-frame cases.
const CASES: &[(&str, &str)] = &[
    ("fill_sum", "28"),
    ("fill_byte", "6"),
    ("fill_record", "9"),
    ("fill_one", "42"),
];

#[test]
fn array_fill_lowers_runs_native_and_round_trips() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("array-fill.sqlite");
    let source = temp.path().join("array-fill.cdb");
    let projection = temp.path().join("array-fill.export.cdb");
    let rebuilt = temp.path().join("array-fill-rebuilt.sqlite");
    std::fs::write(&source, SOURCE).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);

    // Reference-evaluator oracle for every case, plus the big array (evaluates fine;
    // only its native frame is too large).
    for (entry, expected) in CASES {
        assert_eq!(
            run(&["eval", path(&db), entry]).trim(),
            *expected,
            "eval mismatch for {entry}"
        );
    }
    assert_eq!(run(&["eval", path(&db), "big_len"]).trim(), "0");

    // `[0; 1024]` must type-check and lower (the plan's acceptance for large fills).
    // The value is lowered ONCE (eval-once) and stored into all 1024 slots.
    let big_ir_path = temp.path().join("big.ir.json");
    run(&["emit-ir", path(&db), "big_len", "--out", path(&big_ir_path)]);
    let big_ir = read_json(&big_ir_path);
    let big_ops = ops(&big_ir);
    assert_eq!(
        big_ops.iter().filter(|op| *op == "store").count(),
        1024,
        "[0; 1024] lowers to a store per slot"
    );

    // Eval-once: the fill value is a single lowered const, not one per slot.
    let sum_ir_path = temp.path().join("sum.ir.json");
    run(&["emit-ir", path(&db), "fill_sum", "--out", path(&sum_ir_path)]);
    let sum_ir = read_json(&sum_ir_path);
    let sum_ir_ops = &sum_ir["ir"]["operations"];
    let sevens = sum_ir_ops
        .as_array()
        .unwrap()
        .iter()
        .filter(|op| op["op"] == "const_i64" && op["value"] == "7")
        .count();
    assert_eq!(sevens, 1, "the fill value is evaluated once, not per slot");

    // The `[value; count]` form survives the projection (a checked view).
    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("[7; 4]"), "fill form round-trips: {exported}");
    assert!(exported.contains("[0; 1024]"));
    assert!(exported.contains("[{x: 2, y: 5}; 3]"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    run(&["verify", path(&rebuilt)]);
    assert_eq!(run(&["eval", path(&rebuilt), "fill_sum"]).trim(), "28");

    if can_build_default_native_target() {
        for (entry, expected) in CASES {
            let created = parse_json(&run(&[
                "create-test",
                path(&db),
                &format!("{entry}_native"),
                "--entry",
                entry,
                &format!("--expect-i64={expected}"),
                "--native-required",
                "--json",
            ]));
            assert_eq!(created["status"], "applied", "create-test {entry}");
        }
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed", "native fill report: {report}");
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["unsupported"], 0);
    }
}

#[test]
fn array_fill_rejects_unsound_or_malformed_forms() {
    // (label, program, expected diagnostic substring)
    let cases = [
        (
            "move-only-value",
            "fn bad() -> i64 effects[alloc, state] =\n  let xs: array<string, 2> = [string_new(\"x\"); 2] in 0\n",
            "non-reference Copy",
        ),
        (
            "reference-value",
            "fn bad<'a>(r: &'a i64) -> i64 =\n  let xs: array<&'a i64, 2> = [r; 2] in 0\n",
            "non-reference Copy",
        ),
        (
            "zero-count",
            "fn bad() -> i64 =\n  let xs: array<i64, 1> = [5; 0] in 0\n",
            "at least 1",
        ),
        (
            "non-literal-count",
            "fn bad(n: i64) -> i64 =\n  let xs: array<i64, 4> = [5; n] in 0\n",
            "integer literal",
        ),
        (
            // #10: an unbounded count imported fine, then host-panicked the
            // evaluator ("capacity overflow") and unrolled per-slot lowering.
            "huge-count",
            "fn bad() -> i64 =\n  let xs: array<i64, 18446744073709551615> = [0; 18446744073709551615] in 0\n",
            "exceeds the supported maximum",
        ),
        (
            // The cap also binds the declared TYPE (no fill needed), so a
            // huge frame type can't arrive through a signature either.
            "huge-declared-type",
            "fn bad(a: array<i64, 99999999999>) -> i64 = a[0]\n",
            "exceeds the supported maximum",
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

fn ops(ir: &JsonValue) -> Vec<String> {
    let mut out = Vec::new();
    collect_ops(&ir["ir"]["operations"], &mut out);
    out
}

fn collect_ops(ops: &JsonValue, out: &mut Vec<String>) {
    for op in ops.as_array().unwrap() {
        out.push(op["op"].as_str().unwrap().to_string());
        if let Some(then_block) = op.get("then_block") {
            collect_ops(&then_block["operations"], out);
        }
        if let Some(else_block) = op.get("else_block") {
            collect_ops(&else_block["operations"], out);
        }
        if let Some(arms) = op.get("arms").and_then(JsonValue::as_array) {
            for arm in arms {
                collect_ops(&arm["block"]["operations"], out);
            }
        }
        if let Some(body) = op.get("body") {
            collect_ops(&body["operations"], out);
        }
    }
}
