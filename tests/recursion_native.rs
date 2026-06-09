// Phase 6 (R1) acceptance: recursion and mutual recursion compile to native
// artifacts and match the reference evaluator (the three-way oracle:
// eval == native). Covers self-recursion, multiple recursive calls in one body,
// mutual recursion (a cyclic call graph that `verify` must accept), and a
// recursive function over a recursive `box` heap type (recursion + recursive
// type layout + recursive drop glue). Recursion is created by the importer as a
// `CreateRecursionGroup` migration (SPEC_V3 §6) — members are bound before any
// body is type-checked — and projects back to ordinary `fn`s.

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

/// Import `source`, assert `entry` evaluates to `expected`, `verify`s, and (on a
/// buildable host) compiles to a native binary that returns the same value.
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
        "{name}: evaluator result"
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
        assert_eq!(report["status"], "passed", "{name}: native test status");
        assert_eq!(report["native_mismatches"], 0, "{name}: native mismatches");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": expected.to_string()}),
            "{name}: native actual value"
        );
    }
}

#[test]
fn self_recursive_factorial_compiles_native_and_matches_oracle() {
    check_native(
        "factorial",
        "fn fact(n: i64) -> i64 = if n < 1 then 1 else n * fact(n - 1)\n\
         fn main() -> i64 = fact(5)\n",
        "main",
        120,
    );
}

#[test]
fn self_recursive_fibonacci_with_two_recursive_calls_matches_oracle() {
    // Two recursive calls in one body — the recursion group still resolves both
    // self-references, and both lower to native calls.
    check_native(
        "fibonacci",
        "fn fib(n: i64) -> i64 = if n < 2 then n else fib(n - 1) + fib(n - 2)\n\
         fn main() -> i64 = fib(10)\n",
        "main",
        55,
    );
}

#[test]
fn mutual_recursion_compiles_native_and_verify_accepts_the_cycle() {
    // is_even/is_odd form a 2-cycle in the call graph; `verify` must accept the
    // cyclic graph (effects, borrows) and both compile to native.
    check_native(
        "even_odd",
        "fn is_even(n: i64) -> i64 = if n < 1 then 1 else is_odd(n - 1)\n\
         fn is_odd(n: i64) -> i64 = if n < 1 then 0 else is_even(n - 1)\n\
         fn main() -> i64 = is_even(10) * 10 + is_odd(10)\n",
        "main",
        10,
    );
}

#[test]
fn recursion_over_recursive_box_heap_type_builds_and_drops_natively() {
    // `build` is self-recursive AND returns a recursive `box` heap type. This
    // exercises recursion + recursive type layout + recursive drop glue: a chain
    // of `n` heap nodes is allocated by recursive descent and freed when `tree`
    // leaves scope (a double-free would abort the native run). Regression guard
    // for the recursive-type cycle in reference-region collection.
    check_native(
        "box_recursion",
        "enum Node { empty: unit  next: box<Node> }\n\
         fn build(n: i64) -> Node effects[alloc] =\n\
           if n < 1 then Node::empty(()) else Node::next(box_new(build(n - 1)))\n\
         fn main() -> i64 effects[alloc] = let tree: Node = build(6) in 42\n",
        "main",
        42,
    );
}

#[test]
fn recursion_members_project_back_to_plain_functions() {
    // A recursion group is an internal representation: members export as ordinary
    // `fn`s (no special syntax), so the checked-view projection round-trips.
    let temp = tempdir().unwrap();
    let db = temp.path().join("project.sqlite");
    let src = temp.path().join("project.cdb");
    let export = temp.path().join("project.export.cdb");
    std::fs::write(
        &src,
        "fn ping(n: i64) -> i64 = if n < 1 then 0 else pong(n - 1)\n\
         fn pong(n: i64) -> i64 = if n < 1 then 1 else ping(n - 1)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(exported.contains("fn ping(n: i64) -> i64"), "ping projects: {exported}");
    assert!(exported.contains("fn pong(n: i64) -> i64"), "pong projects: {exported}");
    // Re-importing the projection succeeds (the clique is re-detected).
    let db2 = temp.path().join("project2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export)]);
    run(&["verify", path(&db2)]);
}
