// Phase 10 (R7) acceptance: early exit / error control flow via `return`.
//
// `return <value>` exits the enclosing function early. In this expression-oriented
// language the load-bearing case is a `return` in a NON-tail position — an `if`
// branch or `case` arm whose value flows into a `let` continuation — which
// abandons that continuation (the genuine "exit early" a plain `if` cannot express
// without hoisting). These tests pin: the reference evaluator == native backend
// oracle on early-exit programs (including the tokenizer forcing fixture); drop
// glue across the early-exit edge (an owned `box` live at the `return` is freed
// exactly once — no double-free at runtime, the drop pinned in the lowered IR, the
// no-leak half confirmed by tests/leak_interposer.rs); the fail-closed rejection of
// a `return` in a value/operand position; projection round-trip; and trace parity.

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

fn run_fail(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

/// Recursively collect every lowered-IR op named `op` (ops nest inside `if`/`case`
/// blocks), returning their `type_hash`es. Used to pin the early-exit drop/return.
fn collect_op_types(value: &JsonValue, op: &str, out: &mut Vec<String>) {
    match value {
        JsonValue::Object(map) => {
            if map.get("op").and_then(|v| v.as_str()) == Some(op)
                && let Some(type_hash) = map.get("type_hash").and_then(|v| v.as_str())
            {
                out.push(type_hash.to_string());
            }
            for child in map.values() {
                collect_op_types(child, op, out);
            }
        }
        JsonValue::Array(items) => {
            for child in items {
                collect_op_types(child, op, out);
            }
        }
        _ => {}
    }
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

/// Import `source`, assert the evaluator yields `expected` for `entry`, verify, and
/// (on a supported native target) build+run a native test asserting the same value
/// — the three-way oracle (eval == native, both == expected).
fn check_native(name: &str, source: &str, entry: &str, expected: i64) {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    assert_eq!(
        run(&["eval", path(&db), entry]).trim(),
        expected.to_string(),
        "{name}: evaluator"
    );
    run(&["verify", path(&db)]);
    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            &format!("{name}_native"),
            "--entry",
            entry,
            // `=` form so a negative expectation (e.g. -1) is not read as a flag.
            &format!("--expect-i64={expected}"),
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied", "{name}: create-test");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed", "{name}: native status");
        assert_eq!(report["native_mismatches"], 0, "{name}: native mismatches");
    }
}

#[test]
fn early_return_in_if_branches_native_matches_oracle() {
    // The malformed-input shape: each guard early-returns a classification, and the
    // final `else` falls through. The function fully diverges (every path returns),
    // which the unreachable terminal return + skipped param scaffolds must handle.
    check_native(
        "classify",
        "fn classify(n: i64) -> i64 =\n\
           if n < 0 then return 0\n\
           else if n < 10 then return 1\n\
           else return 2\n\
         fn main() -> i64 = classify(0 - 5) * 100 + classify(3) * 10 + classify(42)\n",
        "main",
        // classify(-5)=0, classify(3)=1, classify(42)=2 -> 0*100 + 1*10 + 2.
        12,
    );
}

#[test]
fn early_return_skips_a_let_continuation_native() {
    // The load-bearing non-tail case: the `return` is an `if` branch whose value
    // feeds a `let`, so taking it abandons the continuation `b * 2 + 1`. A plain
    // `if` could only express this by hoisting the continuation into the else.
    check_native(
        "skip_continuation",
        "fn parse(at_end: i64, byte: i64) -> i64 =\n\
           let b: i64 = (if at_end < 1 then return 0 else byte) in\n\
           b * 2 + 1\n\
         fn main() -> i64 = parse(0, 9) * 1000 + parse(5, 9)\n",
        // parse(0,9): early return 0 (skips b*2+1); parse(5,9): 9*2+1 = 19 -> 0*1000 + 19.
        "main",
        19,
    );
}

#[test]
fn early_return_from_a_case_arm_native() {
    // An enum dispatched by `case`, the `none` arm early-returns a sentinel and
    // skips the `let` continuation — the Result-shaped early exit without generics.
    check_native(
        "unwrap_or",
        "enum Opt { none: unit\n  some: i64 }\n\
         fn unwrap_double(o: Opt) -> i64 =\n\
           let v: i64 = (case o of none(u) => return 0 - 1 | some(x) => x) in\n\
           v * 10\n\
         fn main() -> i64 = unwrap_double(Opt::some(5)) * 1000 + unwrap_double(Opt::none(()))\n",
        // some(5): 5*10 = 50; none: early return -1 (skips v*10).
        "main",
        50 * 1000 + (0 - 1),
    );
}

#[test]
fn tokenizer_rejects_malformed_input_native_and_matches_oracle() {
    // The Phase 10 forcing fixture (examples/v3/tokenizer.cdb): a recursive decimal
    // tokenizer that early-returns -1 on a non-digit byte, abandoning the rest of
    // the scan. Well-formed "123" -> 123, malformed "1x3" -> -1.
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/v3/tokenizer.cdb"),
    )
    .expect("tokenizer.cdb");
    check_native("tokenizer_ok", &source, "tokenize_ok", 123);
    check_native("tokenizer_bad", &source, "tokenize_bad", -1);
    check_native("tokenizer_empty", &source, "tokenize_empty", 0);
}

#[test]
fn early_return_drops_a_live_box_across_the_edge_native() {
    // SPEC_V3 §7: an owned `box` local is live at an early `return` on one path and
    // at scope exit on the other. Lowering must drop it on BOTH — once each — so the
    // built binary runs (a double free would abort) and the evaluator agrees. The
    // lowered IR carries a `free_box_shell`/`drop` for the box inside the divergent
    // (early-return) branch, proving the drop is placed on the early-exit edge.
    let source = "record Node { v: i64 }\n\
                  fn pick(flag: i64) -> i64 effects[alloc] =\n\
                    let h: box<Node> = box_new({ v: 99 }) in\n\
                    if flag < 1 then return 7\n\
                    else 8\n\
                  fn main() -> i64 effects[alloc] = pick(0) * 10 + pick(5)\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("box_edge.sqlite");
    let src = temp.path().join("box_edge.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    // pick(0): early return 7, drops h; pick(5): 8, drops h at scope exit. 7*10+8 = 78.
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "78");
    run(&["verify", path(&db)]);

    // The early-exit branch contains the box drop (a `drop` of the box pointer, then
    // a free), pinning the drop placement on the diverging edge rather than only at
    // the fall-through scope exit.
    let ir_file = temp.path().join("pick.ir.json");
    run(&["emit-ir", path(&db), "pick", "--out", path(&ir_file)]);
    let ir = parse_json(&std::fs::read_to_string(&ir_file).unwrap());
    let mut early = Vec::new();
    collect_op_types(&ir, "early_return", &mut early);
    assert_eq!(early.len(), 1, "pick lowers to exactly one early_return: {ir}");
    let mut drops = Vec::new();
    collect_op_types(&ir, "drop", &mut drops);
    assert!(
        drops.len() >= 2,
        "the box is dropped on the early-exit edge AND the fall-through path: {ir}"
    );

    if can_build_default_native_target() {
        // A double free would abort the process; a successful run with exit 78 shows
        // exactly-once drop on both the early-exit and fall-through paths.
        let exe = temp.path().join("box_edge.exe");
        run(&["build", path(&db), "main", "--out", path(&exe)]);
        let status = StdCommand::new(&exe).status().expect("run box_edge");
        assert_eq!(status.code(), Some(78), "native early-exit drop run");
    }
}

#[test]
fn return_in_a_value_position_is_rejected_fail_closed() {
    // `return` is well-formed only as a block result (function body, `if`/`case`
    // branch, `let` body). In a strict value position it would consume its own
    // (divergent) value, so it is rejected before typing with a clean diagnostic.
    let cases: &[(&str, &str)] = &[
        ("let_value", "fn f(n: i64) -> i64 = let x: i64 = return n in x\n"),
        (
            "call_arg",
            "fn g(n: i64) -> i64 = n\nfn f(n: i64) -> i64 = g(return n)\n",
        ),
        ("operand", "fn f(n: i64) -> i64 = (return n) + 1\n"),
        ("if_cond", "fn f(n: i64) -> i64 = if (return n) then 1 else 2\n"),
        (
            "fold_body",
            "fn f(xs: array<i64, 2>) -> i64 = fold x in xs with a = 0 do return a\n",
        ),
    ];
    for (label, source) in cases {
        let temp = tempdir().unwrap();
        let db = temp.path().join(format!("{label}.sqlite"));
        let src = temp.path().join(format!("{label}.cdb"));
        std::fs::write(&src, source).unwrap();
        run(&["init", path(&db)]);
        let err = run_fail(&["import", path(&db), path(&src)]);
        assert!(
            err.contains("`return` may only appear"),
            "{label}: expected a return-position rejection, got: {err}"
        );
    }
}

#[test]
fn early_return_round_trips_through_projection() {
    // The `.cdb` projection of a `return` re-parses to the same program (SPEC_V3
    // §11 checked view): export, re-import, and confirm value stability.
    let source = "fn parse(at_end: i64, byte: i64) -> i64 =\n\
                    let b: i64 = (if at_end < 1 then return 0 else byte) in\n\
                    b * 2 + 1\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("rt.sqlite");
    let src = temp.path().join("rt.cdb");
    let export = temp.path().join("rt.export.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(
        exported.contains("if at_end < 1 then return 0 else byte"),
        "projection must render the early return: {exported}"
    );

    let db2 = temp.path().join("rt2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export)]);
    run(&["verify", path(&db2)]);
    assert_eq!(run(&["eval", path(&db2), "parse", "0", "9"]).trim(), "0");
    assert_eq!(run(&["eval", path(&db2), "parse", "5", "9"]).trim(), "19");
}

#[test]
fn early_return_steps_under_trace() {
    // The tracer steps a `return` (the operand is evaluated, then the function exits
    // with that value) — trace stays consistent with the evaluator across the edge.
    let source = "fn parse(at_end: i64) -> i64 =\n\
                    let b: i64 = (if at_end < 1 then return 0 else 9) in\n\
                    b * 2 + 1\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("trace.sqlite");
    let src = temp.path().join("trace.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    // The early-exit path returns 0 (the trace must reach the function's exit value).
    let trace = parse_json(&run(&["trace", path(&db), "parse", "0", "--json"]));
    let exit = find_exit_value(&trace).expect("trace has a function exit value");
    assert_eq!(exit, "0", "early-return trace exit value: {trace}");
}

/// Find the `value` of an `exit_function` trace event (the function's result).
fn find_exit_value(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::Object(map) => {
            if map.get("event").and_then(|v| v.as_str()) == Some("exit_function")
                && let Some(v) = map.get("value").and_then(|v| v.get("value")).and_then(|v| v.as_str())
            {
                return Some(v.to_string());
            }
            map.values().find_map(find_exit_value)
        }
        JsonValue::Array(items) => items.iter().find_map(find_exit_value),
        _ => None,
    }
}
