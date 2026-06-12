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
    kind: String,
    entry: String,
    ret: String,
    body: String,
    expect_flag: String,
    expect: String,
}

const BOOL: &str = "--expect-bool";

/// The sized integer widths, mirroring `SCALAR_INT_TYPES`.
const WIDTHS: &[(&str, u32, bool)] = &[
    ("i8", 1, true),
    ("i16", 2, true),
    ("i32", 4, true),
    ("i64", 8, true),
    ("u8", 1, false),
    ("u16", 2, false),
    ("u32", 4, false),
    ("u64", 8, false),
];

/// Reduce `value` (a full-precision result) into width `width_bytes`, formatted as
/// the decimal text the `--expect-int` flag wants — the wrapping the evaluator and
/// backend both apply.
fn wrap_to_width(value: i128, width_bytes: u32, signed: bool) -> String {
    let bits = width_bytes * 8;
    let modulus = 1i128 << bits;
    let masked = value.rem_euclid(modulus);
    if signed && masked >= (1i128 << (bits - 1)) {
        (masked - modulus).to_string()
    } else {
        masked.to_string()
    }
}

/// One fixture per registered operator. The boolean operators are fixed; the
/// integer operators are generated for every width, so the coverage gate stays
/// satisfied as widths are added. Operands `a = 12`, `b = 3` keep arithmetic/
/// bitwise/shift results in range for every width (only negate/complement of an
/// unsigned width wrap, handled by `wrap_to_width`).
fn fixtures() -> Vec<Fixture> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<Fixture>, kind: String, ret: &str, body: String, expect_flag: &str, expect: String| {
        out.push(Fixture {
            entry: format!("op_{kind}"),
            kind,
            ret: ret.to_string(),
            body,
            expect_flag: expect_flag.to_string(),
            expect,
        });
    };

    // Boolean operators.
    push(&mut out, "and_bool".into(), "bool", "true && false".into(), BOOL, "false".into());
    push(&mut out, "or_bool".into(), "bool", "true || false".into(), BOOL, "true".into());
    push(&mut out, "not_bool".into(), "bool", "!false".into(), BOOL, "true".into());

    let (a, b): (i128, i128) = (12, 3);
    for &(t, width, signed) in WIDTHS {
        let int_flag = "--expect-int";
        let bind = |expr: &str| format!("let x: {t} = 12 in let y: {t} = 3 in {expr}");
        // Arithmetic / bitwise / shift -> same width.
        let binops: &[(&str, &str, i128)] = &[
            ("add", "+", a + b),
            ("sub", "-", a - b),
            ("mul", "*", a * b),
            ("div", "/", a / b),
            ("mod", "%", a % b),
            ("and", "&", a & b),
            ("or", "|", a | b),
            ("xor", "^", a ^ b),
            ("shl", "<<", a << b),
            ("shr", ">>", a >> b),
        ];
        for &(verb, op, result) in binops {
            push(
                &mut out,
                format!("{verb}_{t}"),
                t,
                bind(&format!("x {op} y")),
                int_flag,
                format!("{t}:{}", wrap_to_width(result, width, signed)),
            );
        }
        // Comparisons -> bool.
        let cmps: &[(&str, &str, bool)] = &[
            ("eq", "==", a == b),
            ("ne", "!=", a != b),
            ("lt", "<", a < b),
            ("le", "<=", a <= b),
            ("gt", ">", a > b),
            ("ge", ">=", a >= b),
        ];
        for &(verb, op, result) in cmps {
            push(
                &mut out,
                format!("{verb}_{t}"),
                "bool",
                bind(&format!("x {op} y")),
                BOOL,
                result.to_string(),
            );
        }
        // Unary negate / bitwise complement -> same width.
        push(
            &mut out,
            format!("neg_{t}"),
            t,
            format!("let x: {t} = 12 in -x"),
            int_flag,
            format!("{t}:{}", wrap_to_width(-a, width, signed)),
        );
        push(
            &mut out,
            format!("bitnot_{t}"),
            t,
            format!("let x: {t} = 12 in ~x"),
            int_flag,
            format!("{t}:{}", wrap_to_width(!a, width, signed)),
        );
    }

    // Edge operands the safe (12, 3) pair never reaches. MIN / -1 (and MIN % -1)
    // is the x86_64 `idiv` #DE case (#8): eval and arm64 wrap to (MIN, 0), so
    // x86_64 must too instead of SIGFPEing. MIN is built by shifting (1 << bits-1
    // wraps) so the fixture does not depend on MIN literals. Distinct `entry`
    // names keep one runnable test per fixture; the `kind` stays the registered
    // operator so the lowering proof and the coverage gate keep working.
    for &(t, width, signed) in WIDTHS {
        if !signed {
            continue;
        }
        let int_flag = "--expect-int";
        let bits = width * 8;
        let min: i128 = -(1i128 << (bits - 1));
        let bind_min = |expr: &str| {
            format!(
                "let one: {t} = 1 in let m: {t} = one << {} in let n: {t} = -1 in {expr}",
                bits - 1
            )
        };
        let edges: &[(&str, &str, i128)] = &[
            ("div_min_neg1", "m / n", -min), // wraps back to MIN
            ("mod_min_neg1", "m % n", 0),
            ("div_min_pos1", "m / one", min),
            ("mod_neg7_pos2", "(0 - 7) % 2", -7 % 2),
        ];
        for &(label, expr, result) in edges {
            let verb = if expr.contains('%') { "mod" } else { "div" };
            push(
                &mut out,
                format!("{verb}_{t}"),
                t,
                bind_min(expr),
                int_flag,
                format!("{t}:{}", wrap_to_width(result, width, signed)),
            );
            // Re-label the entry so each edge case is its own function/test.
            let fixture = out.last_mut().expect("just pushed");
            fixture.entry = format!("op_{label}_{t}");
        }
    }
    out
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
    let mut covered: Vec<String> = fixtures().iter().map(|fixture| fixture.kind.clone()).collect();
    covered.sort();
    covered.dedup();
    let mut registered: Vec<String> =
        codedb::operator_kinds().iter().map(|k| k.to_string()).collect();
    registered.sort();
    assert_eq!(
        covered, registered,
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
        run(&["emit-ir", path(&db), fixture.entry.as_str(), "--out", path(&ir_path)]);
        let ir = read_json(&ir_path);
        assert!(
            ir_contains_kind(&ir, &fixture.kind),
            "fixture {} did not lower to a {} op",
            fixture.entry,
            fixture.kind
        );
    }

    // Step 2: register a native-required test per fixture (reference + native).
    // Named by entry, not kind: the edge-operand fixtures share an operator kind
    // with the base fixture but are separate runnable cases.
    for fixture in &fixtures {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            &format!("conf_{}", fixture.entry),
            "--entry",
            fixture.entry.as_str(),
            &format!("{}={}", fixture.expect_flag, fixture.expect),
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied", "create-test {}", fixture.entry);
    }

    // Step 3: one run executes reference and native for every operator.
    let report = parse_json(&run(&["test", path(&db), "--json"]));
    let tests = report["tests"].as_array().expect("tests array");
    let toolchain = can_build_default_native_target();
    for fixture in &fixtures {
        let name = format!("conf_{}", fixture.entry);
        let test = tests
            .iter()
            .find(|test| test["name"].as_str() == Some(name.as_str()))
            .unwrap_or_else(|| panic!("missing test {name}"));
        assert_eq!(
            test["reference"]["status"], "passed",
            "reference evaluator disagreed on {}",
            fixture.entry
        );
        if toolchain {
            assert_eq!(
                test["native"]["status"], "passed",
                "native backend disagreed with the evaluator on {}",
                fixture.entry
            );
        } else {
            // No toolchain: the native-required gate must fire, never pass vacuously.
            assert_eq!(
                test["native"]["status"], "unsupported",
                "native-required gate must fire without a toolchain ({})",
                fixture.entry
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
