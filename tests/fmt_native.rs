// Phase 12 (R3): integer <-> string formatting as stdlib (`std/fmt.cdb`) over the
// dynamic string buffer. The oracle is eval == native; the round-trip covers the
// full i64 range including i64::MIN (which has no positive magnitude, so the
// formatter/parser work in the negative domain). "No hand-rolled digit table":
// the digit codec is `'0' + d` / `b - '0'` arithmetic, asserted by also pinning
// the exact formatted bytes, not only the round-trip.
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

/// (entry function, expected i64 result)
const CASES: &[(&str, &str)] = &[
    ("rt_zero", "0"),
    ("rt_pos", "12345"),
    ("rt_neg", "-9876"),
    ("rt_max", "9223372036854775807"),
    ("rt_min", "-9223372036854775808"),
    // Format-byte pins (not just round-trip): "-9876" is 5 bytes, byte 0 is '-'
    // (45), byte 1 is '9' (57); str(MIN) is exactly 20 bytes ('-' + 19 digits).
    ("neg_len", "5"),
    ("neg_first", "45"),
    ("neg_second", "57"),
    ("min_len", "20"),
    ("zero_len", "1"),
];

const DRIVER: &str = r#"
fn rt_zero() -> i64 effects[state, alloc] = std.fmt.string_to_i64(std.fmt.i64_to_string(0))
fn rt_pos() -> i64 effects[state, alloc] = std.fmt.string_to_i64(std.fmt.i64_to_string(12345))
fn rt_neg() -> i64 effects[state, alloc] = std.fmt.string_to_i64(std.fmt.i64_to_string(0 - 9876))
fn rt_max() -> i64 effects[state, alloc] =
  std.fmt.string_to_i64(std.fmt.i64_to_string(9223372036854775807))
fn rt_min() -> i64 effects[state, alloc] =
  std.fmt.string_to_i64(std.fmt.i64_to_string(0 - 9223372036854775807 - 1))
fn neg_len() -> i64 effects[state, alloc] =
  let s: string = std.fmt.i64_to_string(0 - 9876) in string_len(s)
fn neg_first() -> i64 effects[state, alloc] =
  let s: string = std.fmt.i64_to_string(0 - 9876) in to_i64(string_get(s, 0))
fn neg_second() -> i64 effects[state, alloc] =
  let s: string = std.fmt.i64_to_string(0 - 9876) in to_i64(string_get(s, 1))
fn min_len() -> i64 effects[state, alloc] =
  let s: string = std.fmt.i64_to_string(0 - 9223372036854775807 - 1) in string_len(s)
fn zero_len() -> i64 effects[state, alloc] =
  let s: string = std.fmt.i64_to_string(0) in string_len(s)
"#;

#[test]
fn fmt_int_to_string_round_trips_native_over_full_i64_range() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("fmt.sqlite");
    let driver = temp.path().join("fmt_driver.cdb");
    std::fs::write(&driver, DRIVER).unwrap();

    run(&["init", path(&db)]);
    // The real library file is the source of truth; the driver composes onto it.
    run(&["import", path(&db), "std/fmt.cdb"]);
    run(&["import", path(&db), path(&driver)]);
    run(&["verify", path(&db)]);

    // Reference-evaluator oracle: every case agrees before we go native.
    for (entry, expected) in CASES {
        assert_eq!(
            run(&["eval", path(&db), entry]).trim(),
            *expected,
            "eval mismatch for {entry}"
        );
    }

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
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed", "native fmt report: {report}");
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["unsupported"], 0);
    } else {
        // Without a native toolchain the eval oracle above still gates the logic.
        assert!(report["tests"].as_array().is_some());
    }
}
