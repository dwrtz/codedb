// Phase 11 (R8) acceptance: condition-driven loops via
// `loop acc = init while cond do body`.
//
// A `loop` carries one accumulator: `acc` starts at `init`; while `cond(acc)`
// holds, `acc` becomes `body(acc)`; the loop yields the final `acc`. It is the
// condition-driven counterpart of `fold` (lowering to a real backend loop) and the
// substrate for fixpoint/worklist passes. These tests pin: the reference evaluator
// == native backend oracle on numeric fixpoints, a record accumulator (a worklist
// iterating an array by dynamic index), and the zero-iteration edge; the
// fail-closed rejection of a move-only accumulator and of a body that moves an
// owned value (loop-carried drop glue is a follow-on, like `fold`); projection
// round-trip; and trace parity.

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

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

/// Import `source`, assert the evaluator yields `expected` for `entry`, verify, and
/// (on a supported native target) build+run a native test asserting the same value.
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
fn scalar_fixpoint_loop_native_matches_oracle() {
    // The smallest fixpoint: count up to n, and double until >= n.
    check_native(
        "scalar_loop",
        "fn count_to(n: i64) -> i64 = loop acc = 0 while acc < n do acc + 1\n\
         fn double_until(n: i64) -> i64 = loop acc = 1 while acc < n do acc * 2\n\
         fn main() -> i64 = count_to(7) * 100 + double_until(5)\n",
        // count_to(7) = 7; double_until(5): 1,2,4,8 -> 8. 7*100 + 8.
        "main",
        708,
    );
}

#[test]
fn record_accumulator_loop_native_matches_oracle() {
    // A record accumulator carrying two loop-varying fields (a Collatz step count):
    // the body builds the next accumulator record, anchored to the named type.
    check_native(
        "collatz",
        "record St { n: i64\n  steps: i64 }\n\
         fn collatz_steps(start: i64) -> i64 =\n\
           let done: St =\n\
             loop s = { n: start, steps: 0 } while s.n > 1 do\n\
               { n: (if s.n % 2 == 0 then s.n / 2 else 3 * s.n + 1), steps: s.steps + 1 }\n\
           in done.steps\n\
         fn main() -> i64 = collatz_steps(6) * 100 + collatz_steps(7)\n",
        // collatz(6): 6,3,10,5,16,8,4,2,1 = 8 steps; collatz(7) = 16 steps.
        "main",
        8 * 100 + 16,
    );
}

#[test]
fn worklist_loop_indexes_an_array_native() {
    // A worklist-style pass: iterate an index over a fixed array, accumulating a
    // running total read by dynamic index — the fixpoint/worklist acceptance shape.
    check_native(
        "worklist",
        "record Cursor { i: i64\n  total: i64 }\n\
         fn sum_array(xs: array<i64, 5>) -> i64 =\n\
           let done: Cursor =\n\
             loop c = { i: 0, total: 0 } while c.i < 5 do\n\
               { i: c.i + 1, total: c.total + xs[c.i] }\n\
           in done.total\n\
         fn main() -> i64 = sum_array([10, 20, 30, 40, 50])\n",
        "main",
        150,
    );
}

#[test]
fn sized_int_accumulator_loop_native_matches_oracle() {
    // A u32 accumulator with wrapping arithmetic and a u32 array read by the loop
    // index — the codec-shaped loop (e.g. a hash round loop). The record-literal init
    // `{ acc: 0x0, i: 0 }` anchors `acc` to the declared u32 width via the named type.
    check_native(
        "u32_loop",
        "record U { acc: u32\n  i: i64 }\n\
         fn fold_xor(xs: array<u32, 4>) -> u32 =\n\
           let done: U = loop s = { acc: 0x0, i: 0 } while s.i < 4 do\n\
             { acc: s.acc ^ xs[s.i], i: s.i + 1 }\n\
           in done.acc\n\
         fn main() -> i64 = to_i64(fold_xor([1, 2, 4, 8]))\n",
        // 1 ^ 2 ^ 4 ^ 8 = 15.
        "main",
        15,
    );
}

#[test]
fn zero_iteration_loop_yields_init_native() {
    // A loop whose condition is immediately false runs the body zero times and
    // yields the initial accumulator unchanged.
    check_native(
        "zero_iter",
        "fn clamp_down(n: i64) -> i64 = loop acc = n while acc > 100 do acc - 1\n\
         fn main() -> i64 = clamp_down(42)\n",
        // 42 is not > 100, so the body never runs; the loop yields 42.
        "main",
        42,
    );
}

#[test]
fn move_only_accumulator_is_rejected_fail_closed() {
    // The accumulator must be copyable: a loop-carried owned value would need
    // loop-aware drop glue (a follow-on), so a move-only accumulator fails closed.
    let source = "record Node { v: i64 }\n\
                  fn go(n: i64) -> box<Node> effects[alloc] =\n\
                    loop acc = box_new({ v: 0 }) while n < 1 do acc\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("moveonly.sqlite");
    let src = temp.path().join("moveonly.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        err.contains("loop accumulator type must be copyable"),
        "expected a move-only accumulator rejection, got: {err}"
    );
}

#[test]
fn loop_body_that_moves_an_owned_value_is_rejected_fail_closed() {
    // A loop cond/body runs 0..N times, so moving an owned place from it would move
    // more than once (double-free) — rejected at TYPECHECK (#14a), so import, the
    // reference evaluator, and verify agree with the native-lowering gate instead of
    // eval happily re-running the move. Each case must hit the dedicated gate
    // message, not some incidental earlier rejection.
    let cases: &[(&str, &str)] = &[
        (
            // The review repro: moving an outer box param from the body imported
            // cleanly before the gate (the borrow checker saw only one move).
            "outer_param_move",
            "record Node { v: i64 }\n\
             fn drop_it(b: box<Node>) -> i64 effects[alloc] = let n: Node = unbox(b) in n.v\n\
             fn go(outer: box<Node>, n: i64) -> i64 effects[alloc] =\n\
               loop acc = 0 while acc < n do acc + drop_it(outer)\n",
        ),
        (
            // A per-iteration let-bound box move is also unlowerable today (no
            // loop-carried drop glue); the typecheck gate mirrors lowering exactly.
            "iteration_local_move",
            "record Node { v: i64 }\n\
             fn drop_it(b: box<Node>) -> i64 effects[alloc] = let n: Node = unbox(b) in n.v\n\
             fn go(n: i64) -> i64 effects[alloc] =\n\
               loop acc = 0 while acc < n do\n\
                 let b: box<Node> = box_new({ v: 1 }) in acc + drop_it(b)\n",
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
            err.contains("loop body cannot move owned values"),
            "{label}: expected the loop-body move gate, got: {err}"
        );
    }
}

#[test]
fn loop_body_consuming_an_rvalue_temporary_stays_lowerable() {
    // The move gate is about PLACES (params/locals): consuming a freshly built
    // rvalue box per iteration never re-moves storage that outlives an iteration,
    // and lowering accepts it — so typecheck must too (eval==native envelope).
    let source = "record Node { v: i64 }\n\
                  fn consume(b: box<Node>) -> i64 effects[alloc] = let n: Node = unbox(b) in n.v\n\
                  fn go(n: i64) -> i64 effects[alloc] =\n\
                    loop acc = 0 while acc < n do acc + consume(box_new({ v: 1 }))\n\
                  fn main() -> i64 effects[alloc] = go(3)\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("rvalue.sqlite");
    let src = temp.path().join("rvalue.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "3");
    run(&["verify", path(&db)]);
}

#[test]
fn fold_body_that_moves_an_owned_value_is_rejected_fail_closed() {
    // The fold counterpart of the loop gate: the body runs once per element, so an
    // outer move would repeat. Before #14a this imported and EVALED (the reference
    // evaluator moved the box N times) while native lowering rejected it.
    let source = "record Node { v: i64 }\n\
                  fn drop_it(b: box<Node>) -> i64 effects[alloc] = let n: Node = unbox(b) in n.v\n\
                  fn go(outer: box<Node>, xs: array<i64, 3>) -> i64 effects[alloc] =\n\
                    fold x in xs with a = 0 do a + drop_it(outer)\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("foldmove.sqlite");
    let src = temp.path().join("foldmove.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        err.contains("fold body cannot move owned values"),
        "expected the fold-body move gate, got: {err}"
    );
}

#[test]
fn loop_round_trips_through_projection() {
    // The `.cdb` projection of a `loop` re-parses to the same program (SPEC_V3 §11).
    let source = "fn count_to(n: i64) -> i64 = loop acc = 0 while acc < n do acc + 1\n";
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
        exported.contains("loop acc = 0 while acc < n do acc + 1"),
        "projection must render the loop: {exported}"
    );

    let db2 = temp.path().join("rt2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export)]);
    run(&["verify", path(&db2)]);
    assert_eq!(run(&["eval", path(&db2), "count_to", "9"]).trim(), "9");
}

#[test]
fn loop_steps_under_trace() {
    // The tracer steps a `loop` (binding the accumulator, evaluating cond then body
    // per iteration) and reaches the function's exit value, consistent with eval.
    let source = "fn count_to(n: i64) -> i64 = loop acc = 0 while acc < n do acc + 1\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("trace.sqlite");
    let src = temp.path().join("trace.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let trace = parse_json(&run(&["trace", path(&db), "count_to", "4", "--json"]));
    let exit = find_exit_value(&trace).expect("trace has a function exit value");
    assert_eq!(exit, "4", "loop trace exit value: {trace}");
}

/// Find the `value` of an `exit_function` trace event (the function's result).
fn find_exit_value(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::Object(map) => {
            if map.get("event").and_then(|v| v.as_str()) == Some("exit_function")
                && let Some(v) = map
                    .get("value")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
            {
                return Some(v.to_string());
            }
            map.values().find_map(find_exit_value)
        }
        JsonValue::Array(items) => items.iter().find_map(find_exit_value),
        _ => None,
    }
}
