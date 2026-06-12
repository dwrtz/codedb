// Phase 9 (R5/R4/R6) acceptance: sized integer operators, native sign-extension,
// hex literals, and numeric cast builtins compile to native artifacts and agree
// with the reference evaluator. The exhaustive per-operator/-width agreement lives
// in `tests/oracle_conformance.rs`; this file covers the integration concerns that
// harness does not: signed-narrow sign-extension, hex literal parsing, the cast
// builtins, literal-width inference in operands, and projection round-trip.

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

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

/// Each case: a zero-arg entry, its `--expect-*` flag, and the expected display.
/// Every case is checked against the evaluator and (when a toolchain exists) the
/// native backend; the whole program is also exported and re-imported to prove
/// the projection round-trips.
struct Case {
    entry: &'static str,
    decl: &'static str,
    expect_flag: &'static str,
    expect: &'static str,
    eval: &'static str,
}

fn check(cases: &[Case]) {
    let temp = tempdir().unwrap();
    let db = temp.path().join("sized.sqlite");
    let src = temp.path().join("sized.cdb");
    let projection = temp.path().join("sized.out.cdb");
    let rebuilt = temp.path().join("sized.rebuilt.sqlite");

    let mut source = String::new();
    for case in cases {
        source.push_str(case.decl);
        source.push('\n');
    }
    std::fs::write(&src, &source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);

    for case in cases {
        assert_eq!(
            run(&["eval", path(&db), case.entry]).trim(),
            case.eval,
            "evaluator disagreed on {}",
            case.entry
        );
    }

    // Projection round-trip: export, re-import, re-export, and the source is stable.
    run(&["export", path(&db), "--branch", "main", "--out", path(&projection)]);
    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    let reexport = temp.path().join("sized.out2.cdb");
    run(&["export", path(&rebuilt), "--branch", "main", "--out", path(&reexport)]);
    assert_eq!(
        std::fs::read_to_string(&projection).unwrap(),
        std::fs::read_to_string(&reexport).unwrap(),
        "projection is not a round-trip fixpoint"
    );

    if !can_build_default_native_target() {
        return;
    }
    for case in cases {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            &format!("{}_native", case.entry),
            "--entry",
            case.entry,
            &format!("{}={}", case.expect_flag, case.expect),
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied", "create-test {}", case.entry);
    }
    let report = parse_json(&run(&["test", path(&db), "--json"]));
    assert_eq!(report["status"], "passed", "native run: {report}");
    assert_eq!(report["native_mismatches"], 0, "native mismatch: {report}");
}

#[test]
fn signed_narrow_sign_extension_agrees_with_native() {
    // The discriminating cases for sign-extension: a negative narrow value must
    // load sign-extended (not zero-extended), so `< 0` is true and arithmetic /
    // shifts keep the sign. A zero-extending load would make these disagree.
    check(&[
        Case {
            entry: "neg_i8",
            decl: "fn neg_i8() -> i8 = let x: i8 = 0 - 5 in x",
            expect_flag: "--expect-int",
            expect: "i8:-5",
            eval: "-5",
        },
        Case {
            entry: "i16_lt_zero",
            decl: "fn i16_lt_zero() -> bool = let x: i16 = 0 - 1 in x < 0",
            expect_flag: "--expect-bool",
            expect: "true",
            eval: "true",
        },
        Case {
            entry: "i32_arith_shift",
            decl: "fn i32_arith_shift() -> i32 = let x: i32 = 0 - 256 in x >> 4",
            expect_flag: "--expect-int",
            expect: "i32:-16",
            eval: "-16",
        },
        Case {
            entry: "i32_add_keeps_sign",
            decl: "fn i32_add_keeps_sign() -> i32 = let x: i32 = 0 - 1000 in x + 1",
            expect_flag: "--expect-int",
            expect: "i32:-999",
            eval: "-999",
        },
    ]);
}

#[test]
fn hex_literals_agree_with_native() {
    check(&[
        Case {
            entry: "hx_u32",
            decl: "fn hx_u32() -> u32 = 0x6a09e667",
            expect_flag: "--expect-int",
            expect: "u32:1779033703",
            eval: "1779033703",
        },
        Case {
            entry: "hx_mask",
            decl: "fn hx_mask() -> u32 = let x: u32 = 0x12345678 in x & 0xff",
            expect_flag: "--expect-int",
            expect: "u32:120",
            eval: "120",
        },
        Case {
            entry: "hx_or",
            decl: "fn hx_or() -> u32 = 0xff00 | 0x00ff",
            expect_flag: "--expect-int",
            expect: "u32:65535",
            eval: "65535",
        },
        Case {
            entry: "hx_i64",
            decl: "fn hx_i64() -> i64 = 0xff",
            expect_flag: "--expect-i64",
            expect: "255",
            eval: "255",
        },
    ]);
}

#[test]
fn numeric_casts_agree_with_native() {
    check(&[
        Case {
            entry: "widen_u8_u32",
            decl: "fn widen_u8_u32() -> u32 = let b: u8 = 200 in to_u32(b)",
            expect_flag: "--expect-int",
            expect: "u32:200",
            eval: "200",
        },
        Case {
            entry: "narrow_u32_u8",
            decl: "fn narrow_u32_u8() -> u8 = let x: u32 = 305419896 in to_u8(x)",
            expect_flag: "--expect-int",
            expect: "u8:120",
            eval: "120",
        },
        Case {
            entry: "reinterpret_u8_i8",
            decl: "fn reinterpret_u8_i8() -> i8 = let x: u8 = 200 in to_i8(x)",
            expect_flag: "--expect-int",
            expect: "i8:-56",
            eval: "-56",
        },
        Case {
            entry: "widen_i8_i32",
            decl: "fn widen_i8_i32() -> i32 = let x: i8 = 0 - 5 in to_i32(x)",
            expect_flag: "--expect-int",
            expect: "i32:-5",
            eval: "-5",
        },
        Case {
            entry: "widen_i32_u64",
            decl: "fn widen_i32_u64() -> u64 = let x: i32 = 0 - 1 in to_u64(x)",
            expect_flag: "--expect-int",
            expect: "u64:18446744073709551615",
            eval: "18446744073709551615",
        },
    ]);
}

#[test]
fn rotate_and_wrapping_arithmetic_agree_with_native() {
    check(&[
        Case {
            entry: "rotr_u32",
            // (x >> 8) | (x << 24) — a 32-bit rotate, the sha256/fnv1a idiom.
            decl: "fn rotr_u32() -> u32 = let x: u32 = 305419896 in (x >> 8) | (x << 24)",
            expect_flag: "--expect-int",
            expect: "u32:2014458966",
            eval: "2014458966",
        },
        Case {
            entry: "wrap_mul_u32",
            decl: "fn wrap_mul_u32() -> u32 = let a: u32 = 16777619 in let b: u32 = 2166136261 in a * b",
            expect_flag: "--expect-int",
            expect: "u32:84696351",
            eval: "84696351",
        },
        Case {
            entry: "wrap_add_u8",
            decl: "fn wrap_add_u8() -> u8 = let a: u8 = 200 in let b: u8 = 100 in a + b",
            expect_flag: "--expect-int",
            expect: "u8:44",
            eval: "44",
        },
    ]);
}

#[test]
fn min_literals_and_signed_hex_bit_patterns_agree_with_native() {
    // #9: the two literal families that were previously unwritable. (1) A
    // negated literal whose positive half overflows the width (`-128` as i8,
    // i64::MIN) folds into one negative literal node at typing and renders
    // back as the unary form (projection round-trip is part of `check`).
    // (2) A hex literal at a signed width is a bit pattern: `0xff` as i8 is
    // -1, `0x8000000000000000` is i64::MIN.
    check(&[
        Case {
            entry: "min_i64",
            decl: "fn min_i64() -> i64 = -9223372036854775808",
            expect_flag: "--expect-i64",
            expect: "-9223372036854775808",
            eval: "-9223372036854775808",
        },
        Case {
            entry: "min_i8",
            decl: "fn min_i8() -> i8 = -128",
            expect_flag: "--expect-int",
            expect: "i8:-128",
            eval: "-128",
        },
        Case {
            entry: "min_i8_arith",
            decl: "fn min_i8_arith() -> i8 = let x: i8 = -128 in x + 1",
            expect_flag: "--expect-int",
            expect: "i8:-127",
            eval: "-127",
        },
        Case {
            entry: "hex_i8_neg1",
            decl: "fn hex_i8_neg1() -> i8 = 0xff",
            expect_flag: "--expect-int",
            expect: "i8:-1",
            eval: "-1",
        },
        Case {
            entry: "hex_i8_min",
            decl: "fn hex_i8_min() -> i8 = 0x80",
            expect_flag: "--expect-int",
            expect: "i8:-128",
            eval: "-128",
        },
        Case {
            entry: "hex_i64_min",
            decl: "fn hex_i64_min() -> i64 = 0x8000000000000000",
            expect_flag: "--expect-i64",
            expect: "-9223372036854775808",
            eval: "-9223372036854775808",
        },
        Case {
            entry: "hex_i32_pos",
            decl: "fn hex_i32_pos() -> i32 = 0x7fffffff",
            expect_flag: "--expect-int",
            expect: "i32:2147483647",
            eval: "2147483647",
        },
    ]);
}

#[test]
fn eval_and_trace_accept_sized_int_arguments() {
    // The CLI arg parser used to support only i64/bool/unit, so a sized-int
    // entry could not be driven by `eval`/`trace`/`debug` at all.
    let temp = tempdir().unwrap();
    let db = temp.path().join("cliargs.sqlite");
    let src = temp.path().join("cliargs.cdb");
    std::fs::write(
        &src,
        "fn double8(x: i8) -> i8 = x + x\nfn maskit(x: u32) -> u32 = x & 0xff\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    // i8 arithmetic wraps at the width even when fed from the CLI.
    assert_eq!(run(&["eval", path(&db), "double8", "--", "-100"]).trim(), "56");
    assert_eq!(run(&["eval", path(&db), "maskit", "4660"]).trim(), "52");
    let trace = parse_json(&run(&["trace", path(&db), "double8", "7", "--json"]));
    assert_eq!(trace["result"]["value"], "14");
    // Out-of-range still fails with a width-specific message.
    let err = {
        let output = bin()
            .args(["eval", path(&db), "double8", "999"])
            .assert()
            .failure()
            .get_output()
            .clone();
        String::from_utf8(output.stderr).expect("utf8 stderr")
    };
    assert!(err.contains("must be i8"), "got: {err}");
}
