//! Evaluator-vs-backend conformance harness (PLAN_V3 Phase 2).
//!
//! The reference evaluator consumes the typed AST; the native backend consumes
//! the lowered IR. They are two divergent consumers of one semantic model, so
//! they are pinned together at the only place they can be compared directly —
//! the observable result of running the same program both ways.
//!
//! For every built-in operator this harness:
//!   1. proves the fixture actually lowers to the operator's kind (emit-ir),
//!   2. runs it through the reference evaluator AND the native backend and
//!      asserts they agree (the native-required test gate),
//!   3. and a coverage gate asserts there is a fixture for *every* operator the
//!      registry knows (`codedb::operator_kinds()`), so adding an operator
//!      without a conformance fixture fails loudly.
//!
//! A native toolchain is required to exercise the backend half; without one, the
//! native-required gate fires `unsupported` (never a vacuous pass), and the
//! evaluator/lowering halves still run.

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

/// One operator fixture: a zero-arg entry whose body exercises exactly the named
/// lowered `kind`, with the result the evaluator and native backend must agree on.
struct Fixture {
    kind: &'static str,
    entry: &'static str,
    ret: &'static str,
    body: &'static str,
    expect_flag: &'static str,
    expect: &'static str,
}

const fn f(
    kind: &'static str,
    entry: &'static str,
    ret: &'static str,
    body: &'static str,
    expect_flag: &'static str,
    expect: &'static str,
) -> Fixture {
    Fixture {
        kind,
        entry,
        ret,
        body,
        expect_flag,
        expect,
    }
}

const I64: &str = "--expect-i64";
const BOOL: &str = "--expect-bool";

/// One fixture per registered operator. u8 operands are produced by byte-string
/// indexing (`b"a"[0]`), the only way to spell a `u8` value in source today.
fn fixtures() -> Vec<Fixture> {
    vec![
        // i64 arithmetic
        f("add_i64", "op_add_i64", "i64", "2 + 3", I64, "5"),
        f("sub_i64", "op_sub_i64", "i64", "7 - 4", I64, "3"),
        f("mul_i64", "op_mul_i64", "i64", "6 * 7", I64, "42"),
        f("div_i64", "op_div_i64", "i64", "20 / 4", I64, "5"),
        f("mod_i64", "op_mod_i64", "i64", "17 % 5", I64, "2"),
        // i64 comparisons
        f("eq_i64", "op_eq_i64", "bool", "2 == 2", BOOL, "true"),
        f("ne_i64", "op_ne_i64", "bool", "2 != 3", BOOL, "true"),
        f("lt_i64", "op_lt_i64", "bool", "1 < 2", BOOL, "true"),
        f("le_i64", "op_le_i64", "bool", "2 <= 2", BOOL, "true"),
        f("gt_i64", "op_gt_i64", "bool", "3 > 2", BOOL, "true"),
        f("ge_i64", "op_ge_i64", "bool", "3 >= 3", BOOL, "true"),
        // u8 comparisons (operands via byte-string indexing)
        f("eq_u8", "op_eq_u8", "bool", "b\"a\"[0] == b\"a\"[0]", BOOL, "true"),
        f("ne_u8", "op_ne_u8", "bool", "b\"a\"[0] != b\"b\"[0]", BOOL, "true"),
        f("lt_u8", "op_lt_u8", "bool", "b\"a\"[0] < b\"b\"[0]", BOOL, "true"),
        f("le_u8", "op_le_u8", "bool", "b\"a\"[0] <= b\"a\"[0]", BOOL, "true"),
        f("gt_u8", "op_gt_u8", "bool", "b\"b\"[0] > b\"a\"[0]", BOOL, "true"),
        f("ge_u8", "op_ge_u8", "bool", "b\"b\"[0] >= b\"b\"[0]", BOOL, "true"),
        // bool binary
        f("and_bool", "op_and_bool", "bool", "true && false", BOOL, "false"),
        f("or_bool", "op_or_bool", "bool", "true || false", BOOL, "true"),
        // unary
        f("neg_i64", "op_neg_i64", "i64", "-5", I64, "-5"),
        f("not_bool", "op_not_bool", "bool", "!false", BOOL, "true"),
    ]
}

/// The trap helpers (separate from the fixtures): `div_zero` divides by a
/// non-constant zero so the division necessarily happens at run time.
const TRAP_HELPERS: &str = "fn op_zero() -> i64 = 0\nfn op_div_zero() -> i64 = 1 / op_zero()\nfn op_mod_zero() -> i64 = 1 % op_zero()\n";

fn program_source(fixtures: &[Fixture]) -> String {
    let mut source = String::new();
    for fixture in fixtures {
        source.push_str(&format!(
            "fn {}() -> {} = {}\n",
            fixture.entry, fixture.ret, fixture.body
        ));
    }
    source.push_str(TRAP_HELPERS);
    source
}

fn ir_contains_kind(ir: &JsonValue, kind: &str) -> bool {
    ir["ir"]["operations"]
        .as_array()
        .map(|ops| {
            ops.iter().any(|op| {
                matches!(op["op"].as_str(), Some("binary") | Some("unary"))
                    && op["kind"].as_str() == Some(kind)
            })
        })
        .unwrap_or(false)
}

#[test]
fn conformance_fixtures_cover_every_registered_operator() {
    // The honesty gate: every operator the registry knows must have a fixture.
    // Adding an `OPS` row without a fixture here makes these sets diverge.
    let mut covered: Vec<&str> = fixtures().iter().map(|fixture| fixture.kind).collect();
    covered.sort_unstable();
    covered.dedup();
    assert_eq!(
        covered,
        codedb::operator_kinds(),
        "every registered operator needs a conformance fixture (add one to fixtures())"
    );
}

#[test]
fn every_operator_lowers_to_its_kind_and_agrees_across_eval_and_native() {
    let fixtures = fixtures();
    let temp = tempdir().unwrap();
    let db = temp.path().join("oracle-conformance.sqlite");
    let source = temp.path().join("oracle-conformance.cdb");
    std::fs::write(&source, program_source(&fixtures)).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    // Step 1 (host-independent): each fixture provably lowers to its kind, so the
    // agreement check below is exercising the operator it claims to.
    for fixture in &fixtures {
        let ir_path = temp.path().join(format!("{}.ir.json", fixture.entry));
        run(&["emit-ir", path(&db), fixture.entry, "--out", path(&ir_path)]);
        let ir = read_json(&ir_path);
        assert!(
            ir_contains_kind(&ir, fixture.kind),
            "fixture {} did not lower to a {} op",
            fixture.entry,
            fixture.kind
        );
    }

    // Step 2: register a native-required test per operator (reference + native).
    for fixture in &fixtures {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            &format!("conf_{}", fixture.kind),
            "--entry",
            fixture.entry,
            &format!("{}={}", fixture.expect_flag, fixture.expect),
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied", "create-test {}", fixture.kind);
    }

    // Step 3: one run executes reference and native for every operator.
    let report = parse_json(&run(&["test", path(&db), "--json"]));
    let tests = report["tests"].as_array().expect("tests array");
    let toolchain = can_build_default_native_target();
    for fixture in &fixtures {
        let name = format!("conf_{}", fixture.kind);
        let test = tests
            .iter()
            .find(|test| test["name"].as_str() == Some(name.as_str()))
            .unwrap_or_else(|| panic!("missing test {name}"));
        assert_eq!(
            test["reference"]["status"], "passed",
            "reference evaluator disagreed on {}",
            fixture.kind
        );
        if toolchain {
            assert_eq!(
                test["native"]["status"], "passed",
                "native backend disagreed with the evaluator on {}",
                fixture.kind
            );
        } else {
            // No toolchain: the native-required gate must fire, never pass vacuously.
            assert_eq!(
                test["native"]["status"], "unsupported",
                "native-required gate must fire without a toolchain ({})",
                fixture.kind
            );
        }
    }

    if toolchain {
        // Every operator agreed across both consumers.
        assert_eq!(report["status"], "passed");
        assert_eq!(report["native_mismatches"], 0);
    }
}

#[test]
fn division_by_zero_traps_natively() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let db = temp.path().join("oracle-trap.sqlite");
    let source = temp.path().join("oracle-trap.cdb");
    let exe = temp.path().join("div-zero-trap");
    std::fs::write(&source, program_source(&fixtures())).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    run(&["build", path(&db), "op_div_zero", "--out", path(&exe)]);
    let status = StdCommand::new(&exe)
        .status()
        .expect("run div-by-zero trap binary");
    assert!(
        !status.success(),
        "div_i64 by zero must trap at runtime (non-zero/abnormal exit), got {status:?}"
    );
}

#[test]
fn modulo_by_zero_traps_natively() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let db = temp.path().join("oracle-mod-trap.sqlite");
    let source = temp.path().join("oracle-mod-trap.cdb");
    let exe = temp.path().join("mod-zero-trap");
    std::fs::write(&source, program_source(&fixtures())).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    run(&["build", path(&db), "op_mod_zero", "--out", path(&exe)]);
    let status = StdCommand::new(&exe)
        .status()
        .expect("run mod-by-zero trap binary");
    assert!(
        !status.success(),
        "mod_i64 by zero must trap at runtime (non-zero/abnormal exit), got {status:?}"
    );
}

#[test]
fn lowered_ir_hash_is_deterministic_under_the_oracle() {
    // Exercises the determinism-oracle helper (PLAN_V3 Phase 2C) from a test, and
    // confirms the IR-hash rung's artifact is stable across runs.
    let temp = tempdir().unwrap();
    let db = temp.path().join("oracle-determinism.sqlite");
    let source = temp.path().join("oracle-determinism.cdb");
    let first = temp.path().join("first.ir.json");
    let second = temp.path().join("second.ir.json");
    std::fs::write(&source, "fn op_add_i64() -> i64 = 2 + 3\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    run(&["emit-ir", path(&db), "op_add_i64", "--out", path(&first)]);
    run(&["emit-ir", path(&db), "op_add_i64", "--out", path(&second)]);
    let first_hash = read_json(&first)["lowered_ir_hash"].as_str().unwrap().to_string();
    let second_hash = read_json(&second)["lowered_ir_hash"].as_str().unwrap().to_string();
    codedb::oracle::assert_hash_identical(
        "ir-hash",
        ("first", &first_hash),
        ("second", &second_hash),
    )
    .expect("lowered IR hash must be deterministic across runs");
}
