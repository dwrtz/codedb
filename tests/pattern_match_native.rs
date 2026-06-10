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
        100 + 1 + 2 + 0 + 9 + 0,
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
