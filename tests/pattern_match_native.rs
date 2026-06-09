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
