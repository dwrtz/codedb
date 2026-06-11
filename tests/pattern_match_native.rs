// Phase 7 (R14) acceptance: scalar literal `case` patterns with a `_` wildcard
// compile to native artifacts and match the reference evaluator, exhaustiveness
// rejects a non-exhaustive scalar case, and a nested `case` (a `case` in an arm
// body) lowers and runs. Scalar `case` desugars to an `if`/`eq` chain at
// lowering (reusing the backend) but is preserved as a rich typed node, so the
// `.cdb` projection round-trips.

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

/// Recursively collect the `type_hash` of every `drop` op in a lowered-IR JSON tree
/// (ops nest inside `case` arm blocks).
fn collect_drop_types(value: &JsonValue, out: &mut Vec<String>) {
    match value {
        JsonValue::Object(map) => {
            if map.get("op").and_then(|v| v.as_str()) == Some("drop")
                && let Some(type_hash) = map.get("type_hash").and_then(|v| v.as_str())
            {
                out.push(type_hash.to_string());
            }
            for child in map.values() {
                collect_drop_types(child, out);
            }
        }
        JsonValue::Array(items) => {
            for child in items {
                collect_drop_types(child, out);
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
            "--expect-i64",
            &expected.to_string(),
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied", "{name}: create-test");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed", "{name}: native status");
        assert_eq!(report["native_mismatches"], 0, "{name}: native mismatches");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": expected.to_string()})
        );
    }
}

#[test]
fn integer_literal_case_with_wildcard_dispatches_native_and_matches_oracle() {
    // Dispatch on integer literals with a `_` default.
    check_native(
        "int_literal_case",
        "fn classify(n: i64) -> i64 = case n of 0 => 100 | 1 => 200 | 7 => 700 | _ => 999\n\
         fn main() -> i64 = classify(0) + classify(1) + classify(7) + classify(42)\n",
        "main",
        100 + 200 + 700 + 999,
    );
}

#[test]
fn bool_literal_case_is_exhaustive_without_wildcard() {
    // A bool `case` covering both true and false is exhaustive (no `_` needed).
    check_native(
        "bool_case",
        "fn pick(b: bool) -> i64 = case b of true => 11 | false => 22\n\
         fn main() -> i64 = pick(1 < 2) * 100 + pick(2 < 1)\n",
        "main",
        11 * 100 + 22,
    );
}

#[test]
fn nested_scalar_case_lowers_and_runs_native() {
    // A nested pattern: the `_` arm's body is itself a `case` on the same scalar.
    check_native(
        "nested_case",
        "fn classify(n: i64) -> i64 =\n\
           case n of\n\
             0 => 1\n\
           | _ => (case n of 1 => 10 | 2 => 20 | _ => 99)\n\
         fn main() -> i64 = classify(0) + classify(1) + classify(2) + classify(5)\n",
        "main",
        1 + 10 + 20 + 99,
    );
}

#[test]
fn nested_case_in_non_last_arm_round_trips() {
    // A nested `case` in a NON-last arm must project to text that re-parses: the
    // inner case is parenthesized so its `| arm` list is not captured by the outer
    // case (SPEC_V3 §11 checked-view round-trip). Without the parens the export
    // reads back as "default case arm must be last".
    let temp = tempdir().unwrap();
    let db = temp.path().join("nested.sqlite");
    let src = temp.path().join("nested.cdb");
    let export = temp.path().join("nested.export.cdb");
    std::fs::write(
        &src,
        "fn f(n: i64) -> i64 = case n of 0 => (case n of 9 => 1 | _ => 2) | _ => 3\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(
        exported.contains("0 => (case n of 9 => 1 | else => 2)"),
        "nested non-last case must be parenthesized: {exported}"
    );

    // The exported projection re-imports and stays value-stable.
    let db2 = temp.path().join("nested2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export)]);
    run(&["verify", path(&db2)]);
    assert_eq!(run(&["eval", path(&db2), "f", "0"]).trim(), "2");
    assert_eq!(run(&["eval", path(&db2), "f", "5"]).trim(), "3");
}

#[test]
fn default_arm_over_multiple_box_variants_drops_each_under_its_own_layout() {
    // The `_`/default arm covers TWO move-only variants with DIFFERENT payload layouts
    // (box<A>, a one-field record, vs box<B>, a two-field record). The default arm is
    // expanded per-uncovered-variant, so each tag-dispatched arm frees its payload under
    // its OWN variant layout. A single shared drop would mis-lay-out one payload (a
    // miscompile) or leak it. This pins the multi-variant case the single-variant
    // default-arm test (recursion_native) does not reach.
    let source = "record A { a: i64 }\n\
                  record B { x: i64\n  y: i64 }\n\
                  enum E { unitv: unit\n  boxa: box<A>\n  boxb: box<B> }\n\
                  fn mk(tag: i64) -> E effects[alloc] =\n\
                    if tag < 1 then E::boxa(box_new({ a: 7 })) else E::boxb(box_new({ x: 1, y: 2 }))\n\
                  fn classify(e: E) -> i64 effects[alloc] =\n\
                    case e of unitv(u) => 0 | _ => 42\n\
                  fn main() -> i64 effects[alloc] = classify(mk(0)) + classify(mk(2))\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("multivariant.sqlite");
    let src = temp.path().join("multivariant.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);

    // The default arm drops both uncovered box variants, each under its own payload
    // layout — two drops with DISTINCT type hashes (box<A> vs box<B>).
    let ir_path = temp.path().join("classify.ir.json");
    run(&["emit-ir", path(&db), "classify", "--out", path(&ir_path)]);
    let ir = parse_json(&std::fs::read_to_string(&ir_path).unwrap());
    let mut drop_types = Vec::new();
    collect_drop_types(&ir, &mut drop_types);
    drop_types.sort();
    drop_types.dedup();
    assert_eq!(
        drop_types.len(),
        2,
        "the `_` arm must drop both box variants under DISTINCT layouts, got {drop_types:?}"
    );

    // eval + native: value correct and the two layout-specific drops do not double-free
    // (a double-free aborts the native run; a wrong-layout drop corrupts/aborts it).
    check_native("multivariant_defaultarm", source, "main", 84);
}

#[test]
fn non_exhaustive_integer_case_is_rejected() {
    // Exhaustiveness: an i64 `case` with no `_` wildcard is rejected with a
    // deterministic diagnostic.
    let temp = tempdir().unwrap();
    let db = temp.path().join("nonexhaustive.sqlite");
    let src = temp.path().join("nonexhaustive.cdb");
    std::fs::write(
        &src,
        "fn classify(n: i64) -> i64 = case n of 0 => 1 | 1 => 2\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let stderr = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        stderr.contains("not exhaustive"),
        "expected a non-exhaustiveness diagnostic, got: {stderr}"
    );
}

#[test]
fn scalar_case_projection_round_trips() {
    // The scalar `case` is preserved as a typed node, so it projects back to a
    // re-parseable `.cdb` source (the `_` wildcard renders as `else`).
    let temp = tempdir().unwrap();
    let db = temp.path().join("project.sqlite");
    let src = temp.path().join("project.cdb");
    let export = temp.path().join("project.export.cdb");
    std::fs::write(
        &src,
        "fn classify(n: i64) -> i64 = case n of 0 => 100 | 7 => 700 | _ => 999\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(
        exported.contains("case n of 0 => 100 | 7 => 700 | else => 999"),
        "scalar case did not project as expected: {exported}"
    );
    // Re-import the projection and confirm it still evaluates.
    let db2 = temp.path().join("project2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export)]);
    run(&["verify", path(&db2)]);
    assert_eq!(run(&["eval", path(&db2), "classify", "5"]).trim(), "999");
    assert_eq!(run(&["eval", path(&db2), "classify", "0"]).trim(), "100");
}

#[test]
fn range_case_dispatches_native_and_matches_oracle() {
    // R14 range patterns: an inclusive `lo..=hi`, an exclusive `lo..hi`, a
    // negative-bound range, and a bare literal in one i64 `case` with a `_` arm.
    // Boundaries are significant: classify(9) hits the inclusive upper of `1..=9`,
    // classify(100) falls THROUGH the exclusive upper of `10..100` to `_`, and
    // classify(-5) hits the inclusive lower of `-5..0`.
    check_native(
        "range_case",
        "fn classify(n: i64) -> i64 = \
           case n of 0 => 100 | 1..=9 => 1 | 10..100 => 2 | -5..0 => 9 | _ => 0\n\
         fn main() -> i64 = \
           classify(0) + classify(9) + classify(10) + classify(100) + classify(-5) + classify(999)\n",
        "main",
        // classify(0)=100, classify(9)=1, classify(10)=2, classify(100)=0,
        // classify(-5)=9, classify(999)=0  => 112
        112,
    );
}

#[test]
fn range_case_projection_round_trips() {
    // The range pattern is a rich typed node, so the `.cdb` projection re-parses and
    // import -> export -> import reproduces the same root hash (SPEC_V3 §11).
    let temp = tempdir().unwrap();
    let db = temp.path().join("rt.sqlite");
    let src = temp.path().join("rt.cdb");
    std::fs::write(
        &src,
        "fn f(n: i64) -> i64 = case n of 1..=9 => 1 | 10..100 => 2 | -5..0 => 9 | _ => 0\n\
         fn main() -> i64 = f(5)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let root1 = run(&["import", path(&db), path(&src)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("import prints root");
    run(&["verify", path(&db)]);

    let export = temp.path().join("rt.export.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(exported.contains("1..=9 => 1"), "inclusive range projects: {exported}");
    assert!(exported.contains("10..100 => 2"), "exclusive range projects: {exported}");
    assert!(exported.contains("-5..0 => 9"), "negative-bound range projects: {exported}");

    let db2 = temp.path().join("rt2.sqlite");
    run(&["init", path(&db2)]);
    let root2 = run(&["import", path(&db2), path(&export)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("re-import prints root");
    assert_eq!(root1, root2, "range case import->export->import must be a fixpoint");
    run(&["verify", path(&db2)]);
    assert_eq!(run(&["eval", path(&db2), "f", "9"]).trim(), "1");
    assert_eq!(run(&["eval", path(&db2), "f", "100"]).trim(), "0");
}

#[test]
fn range_only_i64_case_without_wildcard_is_rejected() {
    // A finite set of ranges cannot prove full i64 coverage, so a `_` is still required.
    let temp = tempdir().unwrap();
    let db = temp.path().join("ne.sqlite");
    let src = temp.path().join("ne.cdb");
    std::fs::write(
        &src,
        "fn f(n: i64) -> i64 = case n of 1..=9 => 1 | 10..100 => 2\nfn main() -> i64 = f(5)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(err.contains("not exhaustive"), "expected a non-exhaustive error, got: {err}");
}

#[test]
fn empty_range_case_is_rejected() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("em.sqlite");
    let src = temp.path().join("em.cdb");
    std::fs::write(
        &src,
        "fn f(n: i64) -> i64 = case n of 9..1 => 1 | _ => 0\nfn main() -> i64 = f(5)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(err.contains("empty range case pattern"), "expected an empty-range error, got: {err}");
}

#[test]
fn range_case_on_bool_scrutinee_is_rejected() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bl.sqlite");
    let src = temp.path().join("bl.cdb");
    std::fs::write(
        &src,
        "fn f(b: bool) -> i64 = case b of 1..=2 => 1 | _ => 0\nfn main() -> i64 = f(true)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(err.contains("i64 scrutinee"), "expected an i64-scrutinee error, got: {err}");
}

#[test]
fn guarded_wildcard_dispatches_native_and_matches_oracle() {
    // R14 if-guards: guarded wildcards (`_ if g`) appear BEFORE the unguarded
    // catch-all and fall through when their guard is false. First-match order is
    // preserved across the native `if`/`eq` chain and the reference evaluator.
    check_native(
        "guard_wildcard",
        "fn classify(n: i64) -> i64 = \
           case n of _ if n > 10 => 1 | _ if n < 0 => 2 | _ => 0\n\
         fn main() -> i64 = \
           classify(20) * 100 + classify(-5) * 10 + classify(5)\n",
        "main",
        100 + 20,
    );
}

#[test]
fn guarded_literal_falls_through_to_unguarded_arm() {
    // A guarded literal (`5 if flag > 0`) that matches the value but FAILS its guard
    // falls through to a later arm — here the same unguarded literal `5`.
    check_native(
        "guard_fallthrough",
        "fn pick(n: i64, flag: i64) -> i64 = \
           case n of 5 if flag > 0 => 100 | 5 => 200 | _ => 0\n\
         fn main() -> i64 = \
           pick(5, 1) + pick(5, 0) + pick(3, 1)\n",
        "main",
        100 + 200,
    );
}

#[test]
fn guarded_range_dispatches_native_and_matches_oracle() {
    // A guard on a range arm: `1..=100 if n > 50` matches only the upper half;
    // 40 is in-range but fails the guard and falls through, 200 is out of range.
    check_native(
        "guard_range",
        "fn classify(n: i64) -> i64 = \
           case n of 1..=100 if n > 50 => 1 | _ => 0\n\
         fn main() -> i64 = \
           classify(60) * 100 + classify(40) * 10 + classify(200)\n",
        "main",
        100,
    );
}

#[test]
fn guard_can_call_a_pure_function() {
    // A guard may call a pure (effect-free) function; the call is a real call-graph
    // edge, so it must survive import/lowering and run natively.
    check_native(
        "guard_pure_call",
        "fn even(n: i64) -> bool = n - (n / 2) * 2 == 0\n\
         fn classify(n: i64) -> i64 = case n of _ if even(n) => 1 | _ => 0\n\
         fn main() -> i64 = \
           classify(4) * 100 + classify(7) * 10 + classify(8)\n",
        "main",
        100 + 1,
    );
}

#[test]
fn guarded_arms_do_not_prove_exhaustiveness() {
    // Two guarded wildcards that semantically cover every i64 STILL need an
    // unguarded `_`: a guard can never be proven total.
    let temp = tempdir().unwrap();
    let db = temp.path().join("ge.sqlite");
    let src = temp.path().join("ge.cdb");
    std::fs::write(
        &src,
        "fn f(n: i64) -> i64 = case n of _ if n > 0 => 1 | _ if n <= 0 => 2\n\
         fn main() -> i64 = f(5)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        err.contains("not exhaustive"),
        "guarded arms must not prove exhaustiveness, got: {err}"
    );
}

#[test]
fn guard_on_bool_scrutinee_is_rejected() {
    // if-guards are i64-only for now (like ranges); a bool-scrutinee guard fails closed.
    let temp = tempdir().unwrap();
    let db = temp.path().join("gb.sqlite");
    let src = temp.path().join("gb.cdb");
    std::fs::write(
        &src,
        "fn f(b: bool, n: i64) -> i64 = case b of true if n > 0 => 1 | _ => 0\n\
         fn main() -> i64 = f(1 < 2, 1)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        err.contains("i64 scrutinee"),
        "expected an i64-scrutinee error, got: {err}"
    );
}

#[test]
fn guard_on_enum_scrutinee_is_rejected() {
    // if-guards on enum arms fail closed (nested-destructuring + guards is a separate
    // follow-on). The rejection fires before the guard is even type-checked.
    let temp = tempdir().unwrap();
    let db = temp.path().join("genum.sqlite");
    let src = temp.path().join("genum.cdb");
    std::fs::write(
        &src,
        "enum E { a: unit\n  b: i64 }\n\
         fn f(e: E) -> i64 = case e of a if 1 > 0 => 1 | b(v) => v\n\
         fn main() -> i64 = 0\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        err.contains("scalar"),
        "expected a scalar-only-guard error, got: {err}"
    );
}

#[test]
fn guard_case_projection_round_trips() {
    // A guard renders as ` if <expr>` between the pattern and `=>`; the typed node
    // round-trips so import -> export -> import reproduces the root hash (SPEC_V3 §11).
    let temp = tempdir().unwrap();
    let db = temp.path().join("grt.sqlite");
    let src = temp.path().join("grt.cdb");
    std::fs::write(
        &src,
        "fn f(n: i64) -> i64 = case n of 5 if n > 0 => 1 | _ if n < 0 => 2 | _ => 0\n\
         fn main() -> i64 = f(5)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let root1 = run(&["import", path(&db), path(&src)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("import prints root");
    run(&["verify", path(&db)]);

    let export = temp.path().join("grt.export.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(
        exported.contains("if n > 0"),
        "guarded literal projects its guard: {exported}"
    );
    assert!(
        exported.contains("if n < 0"),
        "guarded wildcard projects its guard: {exported}"
    );

    let db2 = temp.path().join("grt2.sqlite");
    run(&["init", path(&db2)]);
    let root2 = run(&["import", path(&db2), path(&export)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("re-import prints root");
    assert_eq!(
        root1, root2,
        "guard case import->export->import must be a fixpoint"
    );
    run(&["verify", path(&db2)]);
    assert_eq!(run(&["eval", path(&db2), "f", "5"]).trim(), "1");
    assert_eq!(run(&["eval", path(&db2), "f", "3"]).trim(), "0");
}

#[test]
fn guard_short_circuits_and_does_not_trap_when_pattern_fails() {
    // SOUNDNESS (eval/native parity): a guard is evaluated ONLY when its pattern
    // matches. `100 / d > 0` would divide by zero — and TRAP — if evaluated eagerly,
    // but for n != 5 the pattern fails and the guard is skipped, so the reference
    // evaluator and the native `if`/`eq` chain BOTH fall through to `_` instead of
    // trapping. The chain places the guard in a then-branch gated on the pattern
    // test (a strict `&&` would have evaluated it eagerly and diverged).
    check_native(
        "guard_short_circuit",
        "fn f(n: i64, d: i64) -> i64 = case n of 5 if 100 / d > 0 => 1 | _ => 0\n\
         fn main() -> i64 = f(3, 0) * 10 + f(5, 2)\n",
        "main",
        1,
    );
}

#[test]
fn effectful_guard_is_allowed_and_accounted_natively() {
    // A guard may use effects (here an inline `alloc`); the effect is accounted in
    // the enclosing function's signature and the guard runs — and frees — natively
    // with eval parity. Short-circuit + balanced alloc/free means no leak, no trap.
    check_native(
        "guard_effectful",
        "fn f(n: i64) -> i64 effects[alloc] = \
           case n of _ if (let b: box<i64> = box_new(n) in true) => 1 | _ => 0\n\
         fn main() -> i64 effects[alloc] = f(5) + f(7)\n",
        "main",
        2,
    );
}

#[test]
fn guard_inline_effect_must_be_declared() {
    // No effect hole: an inline effect inside a guard requires the enclosing function
    // to declare it (`expr_requires_*` walks the guard like the body), and a call-borne
    // guard effect flows through the dependency graph (`collect_expr_deps`).
    let temp = tempdir().unwrap();
    let db = temp.path().join("gd.sqlite");
    let src = temp.path().join("gd.cdb");
    std::fs::write(
        &src,
        "fn f(n: i64) -> i64 = case n of _ if (let b: box<i64> = box_new(n) in true) => 1 | _ => 0\n\
         fn main() -> i64 = f(5)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        err.contains("undeclared effect alloc"),
        "an inline guard effect must be declared, got: {err}"
    );
}

#[test]
fn guarded_arm_body_move_compensates_drop_on_guard_failure() {
    // SOUNDNESS — Phase-4 conditional drop glue UNDER a guard. The guarded arm
    // `5 if flag > 0` MOVES its owned `box` param (via `unbox`); the `_` arm does
    // not. When the guard FAILS (flag <= 0) control falls through to `_`, where the
    // box was never consumed, so a compensating drop (SPEC_V3 §7) must free it
    // exactly once. A miss here leaks (guard-fail path) or double-frees (if both the
    // unbox and a compensating drop ran). The native run aborts on a double-free.
    check_native(
        "guard_drop_compensation",
        "fn f(n: i64, flag: i64, b: box<i64>) -> i64 effects[alloc] = \
           case n of 5 if flag > 0 => unbox(b) | _ => 0\n\
         fn main() -> i64 effects[alloc] = \
           f(5, 1, box_new(10)) + f(5, 0, box_new(20)) + f(3, 1, box_new(30))\n",
        "main",
        10,
    );
}

// ===========================================================================
// R14 nested enum-destructuring patterns: `case o of wrap(has(x)) => ...`.
// A variant's (inline enum) payload is itself matched against an inner variant
// pattern, recursively, binding the chain's single leaf. Lowering builds a
// recursive decision tree (one tag switch per level); the per-level binding +
// residual / merge drop glue keeps every owned value dropped exactly once.
// ===========================================================================

/// Count every `op` of kind `op_name` in a lowered-IR JSON tree, descending into
/// `if`/`case` blocks (ops nest inside arm blocks).
fn count_op(value: &JsonValue, op_name: &str) -> usize {
    let mut count = 0;
    match value {
        JsonValue::Object(map) => {
            if map.get("op").and_then(|v| v.as_str()) == Some(op_name) {
                count += 1;
            }
            for child in map.values() {
                count += count_op(child, op_name);
            }
        }
        JsonValue::Array(items) => {
            for child in items {
                count += count_op(child, op_name);
            }
        }
        _ => {}
    }
    count
}

// A two-level destructuring over distinctly-typed inner variants, plus a simple
// top-level arm. Shared by the dispatch and projection round-trip tests.
const NESTED_BASIC: &str = "enum Sign { pos: i64\n  neg: i64 }\n\
     enum Cell { sign: Sign\n  zero: unit }\n\
     fn classify(c: Cell) -> i64 =\n\
       case c of sign(pos(x)) => x * 2 | sign(neg(y)) => y * 3 | zero(u) => 5\n\
     fn main() -> i64 =\n\
       classify(Cell::sign(Sign::pos(7))) + classify(Cell::sign(Sign::neg(4))) + classify(Cell::zero(()))\n";

#[test]
fn nested_destructuring_dispatches_native_and_matches_oracle() {
    // Two-level dispatch: the outer `sign` variant carries an inline `Sign` enum that
    // is itself matched on `pos`/`neg`, each binding the leaf; `zero` is a simple arm.
    // 7*2 + 4*3 + 5 = 31.
    check_native("nested_basic", NESTED_BASIC, "main", 31);
}

#[test]
fn three_level_nested_destructuring_dispatches_native() {
    // Three levels of inline-enum nesting exercise the decision-tree recursion depth.
    // 5 + 6*10 = 65.
    check_native(
        "nested_three_level",
        "enum L3 { a: i64\n  b: i64 }\n\
         enum L2 { mid: L3 }\n\
         enum L1 { top: L2 }\n\
         fn deep(x: L1) -> i64 = case x of top(mid(a(v))) => v | top(mid(b(w))) => w * 10\n\
         fn main() -> i64 = deep(L1::top(L2::mid(L3::a(5)))) + deep(L1::top(L2::mid(L3::b(6))))\n",
        "main",
        65,
    );
}

#[test]
fn nested_destructuring_with_wildcard_fallback_native() {
    // A nested arm covers one inner variant; the `_` fallback covers the rest. The
    // fallback is expanded per uncovered inner variant at lowering. 1 + 99 = 100.
    check_native(
        "nested_fallback",
        "enum Color { red: unit\n  green: unit\n  blue: unit }\n\
         enum Wrap { w: Color }\n\
         fn name(x: Wrap) -> i64 = case x of w(red(u)) => 1 | _ => 99\n\
         fn main() -> i64 = name(Wrap::w(Color::red(()))) + name(Wrap::w(Color::green(())))\n",
        "main",
        100,
    );
}

#[test]
fn nested_move_only_box_payload_moves_out_through_nesting_native() {
    // A move-only payload (a record owning a box) is destructured TWO levels deep and
    // moved out of a consumed-place scrutinee, then freed once by `consume`/`unbox`.
    // The outer enum is `Move`d whole (never dropped), the inner enum shell is inline,
    // so the box has a single owner. A double-free aborts the native run. = 42.
    check_native(
        "nested_move_only",
        "record Boxed { b: box<i64> }\n\
         enum Inner { has: Boxed\n  none: unit }\n\
         enum Outer { wrap: Inner }\n\
         fn consume(x: Boxed) -> i64 effects[alloc] = unbox(x.b)\n\
         fn f(o: Outer) -> i64 effects[alloc] =\n\
           case o of wrap(has(x)) => consume(x) | wrap(none(u)) => 0\n\
         fn main() -> i64 effects[alloc] = f(Outer::wrap(Inner::has({ b: box_new(42) })))\n",
        "main",
        42,
    );
}

#[test]
fn nested_fallback_drops_uncovered_inner_box_variant() {
    // The `_` fallback covers an uncovered INNER variant whose payload owns a box. That
    // payload must be freed in the fallback arm (the consumed outer enum is never
    // dropped), or it leaks. Pin the static drop, then prove eval+native agree and the
    // drop does not double-free.
    let source = "record Boxed { b: box<i64> }\n\
         enum Inner { has: Boxed\n  other: Boxed }\n\
         enum Outer { wrap: Inner }\n\
         fn consume(x: Boxed) -> i64 effects[alloc] = unbox(x.b)\n\
         fn f(o: Outer) -> i64 effects[alloc] = case o of wrap(has(x)) => consume(x) | _ => 0\n\
         fn main() -> i64 effects[alloc] = f(Outer::wrap(Inner::other({ b: box_new(9) })))\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("nfb.sqlite");
    let src = temp.path().join("nfb.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);

    let ir_path = temp.path().join("f.ir.json");
    run(&["emit-ir", path(&db), "f", "--out", path(&ir_path)]);
    let ir = parse_json(&std::fs::read_to_string(&ir_path).unwrap());
    let mut drops = Vec::new();
    collect_drop_types(&ir, &mut drops);
    assert!(
        !drops.is_empty(),
        "the `_` fallback must free the uncovered inner box variant (leak guard), got {drops:?}"
    );
    check_native("nested_fallback_drop", source, "main", 0);
}

#[test]
fn nested_binding_left_live_drops_its_payload() {
    // A nested leaf binding the body does NOT consume must be dropped at arm-scope exit
    // (mirrors `let`-binding drop placement). `x: Boxed` owns a box; the body `0` never
    // uses it, so a residual drop frees it once — else it leaks. Leak guard.
    let source = "record Boxed { b: box<i64> }\n\
         enum Inner { has: Boxed\n  none: unit }\n\
         enum Outer { wrap: Inner }\n\
         fn f(o: Outer) -> i64 effects[alloc] = case o of wrap(has(x)) => 0 | wrap(none(u)) => 1\n\
         fn main() -> i64 effects[alloc] = f(Outer::wrap(Inner::has({ b: box_new(7) })))\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("nbl.sqlite");
    let src = temp.path().join("nbl.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);

    let ir_path = temp.path().join("f.ir.json");
    run(&["emit-ir", path(&db), "f", "--out", path(&ir_path)]);
    let ir = parse_json(&std::fs::read_to_string(&ir_path).unwrap());
    let mut drops = Vec::new();
    collect_drop_types(&ir, &mut drops);
    assert!(
        !drops.is_empty(),
        "an unconsumed nested binding must drop its box payload (leak guard), got {drops:?}"
    );
    check_native("nested_binding_live", source, "main", 0);
}

#[test]
fn conditional_outer_move_across_nested_arms_drops_exactly_once() {
    // The keystone drop-glue case: an owned param `y` is consumed in ONE inner nested
    // arm but left live in the sibling inner arm AND in the outer sibling. The two-level
    // merge compensation must drop `y` exactly once on each non-consuming path — not
    // zero (leak) and not twice (double-free). Confirmed: 1 move (into `consume` on the
    // wrap(a) path) + 2 compensation drops (wrap(b), plain). A double-free aborts native.
    let source = "record Boxed { b: box<i64> }\n\
         enum Inner { a: unit\n  b: unit }\n\
         enum Outer { wrap: Inner\n  plain: unit }\n\
         fn consume(y: Boxed) -> i64 effects[alloc] = unbox(y.b)\n\
         fn f(o: Outer, y: Boxed) -> i64 effects[alloc] =\n\
           case o of wrap(a(u)) => consume(y) | wrap(b(u)) => 0 | plain(u) => 0\n\
         fn pick(sel: i64) -> Outer =\n\
           if sel < 1 then Outer::wrap(Inner::a(()))\n\
           else if sel < 2 then Outer::wrap(Inner::b(())) else Outer::plain(())\n\
         fn main() -> i64 effects[alloc] =\n\
           f(pick(0), { b: box_new(100) }) + f(pick(1), { b: box_new(200) }) + f(pick(2), { b: box_new(300) })\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("cond.sqlite");
    let src = temp.path().join("cond.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);

    let ir_path = temp.path().join("f.ir.json");
    run(&["emit-ir", path(&db), "f", "--out", path(&ir_path)]);
    let ir = parse_json(&std::fs::read_to_string(&ir_path).unwrap());
    assert_eq!(
        count_op(&ir, "drop"),
        2,
        "y must be compensation-dropped exactly once on each of the two non-consuming paths"
    );
    assert_eq!(
        count_op(&ir, "move"),
        1,
        "y is moved into `consume` only on the wrap(a) path"
    );
    // Only wrap(a) consumes y (unbox = 100); wrap(b) and plain return 0.
    check_native("cond_nested_move", source, "main", 100);
}

#[test]
fn nested_destructuring_projection_round_trips() {
    // The nested pattern is a rich typed node, so the `.cdb` projection re-parses and
    // import -> export -> import reproduces the same root hash (SPEC_V3 §11).
    let temp = tempdir().unwrap();
    let db = temp.path().join("nrt.sqlite");
    let src = temp.path().join("nrt.cdb");
    std::fs::write(&src, NESTED_BASIC).unwrap();
    run(&["init", path(&db)]);
    let root1 = run(&["import", path(&db), path(&src)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("import prints root");
    run(&["verify", path(&db)]);

    let export = temp.path().join("nrt.export.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(
        exported.contains("case c of sign(pos(x)) => x * 2 | sign(neg(y)) => y * 3 | zero(u) => 5"),
        "nested destructuring did not project as expected: {exported}"
    );

    let db2 = temp.path().join("nrt2.sqlite");
    run(&["init", path(&db2)]);
    let root2 = run(&["import", path(&db2), path(&export)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("re-import prints root");
    assert_eq!(
        root1, root2,
        "nested destructuring import->export->import must be a fixpoint"
    );
    run(&["verify", path(&db2)]);
    assert_eq!(run(&["eval", path(&db2), "main"]).trim(), "31");
}

#[test]
fn nested_destructuring_with_wildcard_leaf_projects() {
    // A wildcard leaf `v(inner(_))` ignores the inner payload; it must round-trip as
    // `_` (re-parseable) and stay a fixpoint.
    let temp = tempdir().unwrap();
    let db = temp.path().join("wl.sqlite");
    let src = temp.path().join("wl.cdb");
    std::fs::write(
        &src,
        "enum Inner { a: i64\n  b: i64 }\n\
         enum Outer { w: Inner }\n\
         fn f(o: Outer) -> i64 = case o of w(a(_)) => 1 | w(b(_)) => 2\n\
         fn main() -> i64 = f(Outer::w(Inner::a(5))) + f(Outer::w(Inner::b(6)))\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let root1 = run(&["import", path(&db), path(&src)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("import prints root");
    let export = temp.path().join("wl.export.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(
        exported.contains("case o of w(a(_)) => 1 | w(b(_)) => 2"),
        "wildcard leaf did not project as `_`: {exported}"
    );
    let db2 = temp.path().join("wl2.sqlite");
    run(&["init", path(&db2)]);
    let root2 = run(&["import", path(&db2), path(&export)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("re-import prints root");
    assert_eq!(root1, root2, "wildcard-leaf nested case must be a fixpoint");
    assert_eq!(run(&["eval", path(&db2), "main"]).trim(), "3");
}

#[test]
fn non_exhaustive_nested_case_is_rejected() {
    // Nested exhaustiveness is recursive: a covered outer variant whose inner variants
    // are not all covered (and no `_`) is rejected.
    let temp = tempdir().unwrap();
    let db = temp.path().join("nne.sqlite");
    let src = temp.path().join("nne.cdb");
    std::fs::write(
        &src,
        "enum Inner { a: i64\n  b: i64 }\n\
         enum Outer { w: Inner }\n\
         fn f(o: Outer) -> i64 = case o of w(a(x)) => x\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let stderr = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        stderr.contains("not exhaustive"),
        "expected a nested non-exhaustiveness diagnostic, got: {stderr}"
    );
}

#[test]
fn mixing_binding_and_nested_pattern_for_one_variant_is_rejected() {
    // A variant cannot carry both a simple binding arm and a nested destructuring arm
    // (the binding is a catch-all that would make the nested arm dead).
    let temp = tempdir().unwrap();
    let db = temp.path().join("nmix.sqlite");
    let src = temp.path().join("nmix.cdb");
    std::fs::write(
        &src,
        "enum Inner { a: i64\n  b: i64 }\n\
         enum Outer { w: Inner\n  z: i64 }\n\
         fn f(o: Outer) -> i64 = case o of w(a(x)) => x | w(rest) => 0 | z(n) => n\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let stderr = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        stderr.contains("cannot mix a binding pattern with nested destructuring"),
        "expected a binding/nested mixing diagnostic, got: {stderr}"
    );
}

#[test]
fn duplicate_catch_all_in_nested_pattern_is_rejected() {
    // Two arms binding the SAME nested path's leaf are two catch-alls at that level;
    // the second is dead. Rejected by the well-formedness check, recursively.
    let temp = tempdir().unwrap();
    let db = temp.path().join("ndup.sqlite");
    let src = temp.path().join("ndup.cdb");
    std::fs::write(
        &src,
        "enum Inner { a: i64\n  b: i64 }\n\
         enum Outer { w: Inner }\n\
         fn f(o: Outer) -> i64 = case o of w(a(x)) => x | w(a(y)) => y | w(b(z)) => z\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    let stderr = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        stderr.contains("duplicate catch-all"),
        "expected a duplicate catch-all diagnostic, got: {stderr}"
    );
}

#[test]
fn renaming_a_variant_rewrites_it_inside_a_nested_pattern() {
    // A `rename_variant` reaching a variant used as an INNER nested-pattern variant
    // must rewrite the pattern too, not just the outer arm/constructor — else the
    // re-type-check fails ("unknown variant") and the rename rolls back. Rename the
    // inner `pos` (a variant of `Sign`, matched as `sign(pos(x))`) to `positive`.
    let temp = tempdir().unwrap();
    let db = temp.path().join("rnv.sqlite");
    let src = temp.path().join("rnv.cdb");
    std::fs::write(&src, NESTED_BASIC).unwrap();
    run(&["init", path(&db)]);
    let root = run(&["import", path(&db), path(&src)])
        .lines()
        .find_map(|line| line.strip_prefix("root ").map(str::to_string))
        .expect("import prints root");

    let patch = temp.path().join("rnv.patch.json");
    std::fs::write(
        &patch,
        serde_json::to_string(&json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": { "kind": "type", "name": "Sign" },
            "replace": { "kind": "rename_variant_and_cases", "variant": "pos", "new_name": "positive" }
        }))
        .unwrap(),
    )
    .unwrap();
    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "applied", "inner-variant rename must apply");

    // Behaviour is preserved and the nested pattern + constructor now name `positive`.
    run(&["verify", path(&db)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "31");
    let export = temp.path().join("rnv.export.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(
        exported.contains("sign(positive(x))"),
        "the inner nested-pattern variant must be renamed: {exported}"
    );
    assert!(
        exported.contains("Sign::positive(7)"),
        "the constructor must be renamed: {exported}"
    );
    assert!(
        !exported.contains("pos(x)"),
        "no stale `pos` reference may remain: {exported}"
    );
}
