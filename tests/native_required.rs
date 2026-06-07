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

#[test]
fn native_required_scalar_test_reports_native_outcome_on_every_host() {
    // This test must not be vacuous on a host without a native toolchain: a
    // native-required test either passes through real codegen (native host) or
    // fires the gate as `unsupported` and fails the run (no toolchain). It may
    // never silently pass without exercising the gate.
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-required-pass.sqlite");
    let source = temp.path().join("native-required-pass.cdb");

    std::fs::write(&source, "fn main() -> i64 = 7\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let created = parse_json(&run(&[
        "create-test",
        path(&db),
        "main_returns_7_native",
        "--entry",
        "main",
        "--expect-i64",
        "7",
        "--native-required",
        "--json",
    ]));
    assert_eq!(created["status"], "applied");

    let listed = parse_json(&run(&["test", path(&db), "--list", "--json"]));
    assert_eq!(listed["tests"][0]["mode"], "reference_and_native");
    assert_eq!(listed["tests"][0]["native_agreement"], true);
    assert_eq!(listed["tests"][0]["native_required"], true);
    assert_eq!(listed["tests"][0]["labels"], json!(["v2_native_required"]));

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    assert_eq!(report["schema"], "codedb/test-run/v1");
    assert_eq!(report["tests"][0]["reference"]["status"], "passed");
    assert_eq!(
        report["tests"][0]["native"]["schema"],
        "codedb/native-test-result/v1"
    );

    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["passed"], 1);
        assert_eq!(report["failed"], 0);
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["expected"],
            json!({"kind": "i64", "value": "7"})
        );
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "7"})
        );
    } else {
        // No toolchain: the native-required gate must fire rather than pass.
        assert_eq!(report["status"], "failed");
        assert_eq!(report["passed"], 0);
        assert_eq!(report["unsupported"], 1);
        assert_eq!(report["tests"][0]["native"]["status"], "unsupported");
    }
}

#[test]
fn native_required_scalar_supports_full_i64_range() {
    // Regression: the native scalar agreement must compare the full value, not a
    // process exit status. A value outside 0..=255 was previously permanently
    // "unsupported" (and failed the native-required gate); it must now pass.
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-required-bigrange.sqlite");
    let source = temp.path().join("native-required-bigrange.cdb");

    std::fs::write(&source, "fn main() -> i64 = 70000\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "create-test",
        path(&db),
        "main_returns_70000_native",
        "--entry",
        "main",
        "--expect-i64",
        "70000",
        "--native-required",
        "--json",
    ]);

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    assert_eq!(report["tests"][0]["reference"]["status"], "passed");
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "70000"})
        );
    } else {
        assert_eq!(report["tests"][0]["native"]["status"], "unsupported");
    }
}

#[test]
fn native_scalar_agreement_does_not_alias_through_exit_code() {
    // Soundness regression: the native result must not be compared as an 8-bit
    // exit status. A native value of 263 with expected 7 (263 % 256 == 7) must
    // report a mismatch with actual 263 — never a false pass.
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-alias.sqlite");
    let source = temp.path().join("native-alias.cdb");

    std::fs::write(&source, "fn main() -> i64 = 263\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "create-test",
        path(&db),
        "main_aliases_to_7",
        "--entry",
        "main",
        "--expect-i64",
        "7",
        "--native-required",
        "--json",
    ]);

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["tests"][0]["native"]["status"], "native_mismatch");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "263"})
        );
    } else {
        assert_eq!(report["tests"][0]["native"]["status"], "unsupported");
    }
}

#[test]
fn test_label_filter_selects_native_required_tests() {
    // The `v2_native_required` label is the documented CI selector; `test
    // --label` must run/list only the tests carrying it, leaving the unfiltered
    // run unchanged.
    let temp = tempdir().unwrap();
    let db = temp.path().join("label-filter.sqlite");
    let source = temp.path().join("label-filter.cdb");

    std::fs::write(&source, "fn a() -> i64 = 1\nfn b() -> i64 = 2\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "create-test",
        path(&db),
        "t_a",
        "--entry",
        "a",
        "--expect-i64",
        "1",
        "--native-required",
        "--json",
    ]);
    run(&[
        "create-test",
        path(&db),
        "t_b",
        "--entry",
        "b",
        "--expect-i64",
        "2",
        "--json",
    ]);

    let names = |report: &JsonValue| -> Vec<String> {
        report["tests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|test| test["name"].as_str().unwrap().to_string())
            .collect()
    };

    // No filter: both tests.
    let all = parse_json(&run(&["test", path(&db), "--list", "--json"]));
    assert_eq!(names(&all), vec!["t_a", "t_b"]);

    // List filtered to the native-required label: only t_a.
    let listed = parse_json(&run(&[
        "test",
        path(&db),
        "--list",
        "--label",
        "v2_native_required",
        "--json",
    ]));
    assert_eq!(names(&listed), vec!["t_a"]);

    // Run filtered: only t_a executes.
    let ran = parse_json(&run(&[
        "test",
        path(&db),
        "--label",
        "v2_native_required",
        "--json",
    ]));
    assert_eq!(names(&ran), vec!["t_a"]);

    // Unknown label: nothing runs, run is vacuously passing.
    let none = parse_json(&run(&[
        "test",
        path(&db),
        "--label",
        "no_such_label",
        "--json",
    ]));
    assert!(names(&none).is_empty());
    assert_eq!(none["status"], "passed");
}

#[test]
fn native_required_flag_survives_history_export_import() {
    // The native-required gate is only trustworthy if the flag, mode, and
    // labels round-trip through migration replay; a dropped flag would silently
    // downgrade a v2 acceptance test to reference-only. This is host-independent
    // (no native build), so it always exercises the preservation guarantee.
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-required-replay.sqlite");
    let rebuilt = temp.path().join("native-required-replay-rebuilt.sqlite");
    let source = temp.path().join("native-required-replay.cdb");
    let history = temp.path().join("history.ndjson");

    std::fs::write(&source, "fn main() -> i64 = 7\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "create-test",
        path(&db),
        "main_returns_7_native",
        "--entry",
        "main",
        "--expect-i64",
        "7",
        "--native-required",
        "--json",
    ]);

    let listed = parse_json(&run(&["test", path(&db), "--list", "--json"]));
    assert_eq!(listed["tests"][0]["mode"], "reference_and_native");
    assert_eq!(listed["tests"][0]["native_required"], true);
    assert_eq!(listed["tests"][0]["labels"], json!(["v2_native_required"]));

    run(&["export-history", path(&db), "--out", path(&history)]);
    run(&["init", path(&rebuilt)]);
    run(&["import-history", path(&rebuilt), path(&history)]);

    // The full listing (mode, native_agreement, native_required, labels,
    // expected, schema) must be byte-identical after replay.
    let rebuilt_listed = parse_json(&run(&["test", path(&rebuilt), "--list", "--json"]));
    assert_eq!(rebuilt_listed, listed);
}

#[test]
fn native_required_unsupported_feature_is_a_failed_test() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-required-unsupported.sqlite");
    let source = temp.path().join("native-required-unsupported.cdb");
    let apply = temp.path().join("native-required-unsupported.apply.json");

    std::fs::write(&source, "fn inc(x: i64) -> i64 = x + 1\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    std::fs::write(
        &apply,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "operations": [
                {
                    "kind": "create_test",
                    "name": "inc_arg_native_required",
                    "entry": "inc",
                    "args": [{"kind": "i64", "value": "1"}],
                    "expected": {"kind": "i64", "value": "2"},
                    "mode": "reference_and_native",
                    "native_required": true
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let created = parse_json(&run(&["apply", path(&db), "--json", path(&apply)]));
    assert_eq!(created["status"], "applied");

    let listed = parse_json(&run(&["test", path(&db), "--list", "--json"]));
    assert_eq!(listed["tests"][0]["mode"], "reference_and_native");
    assert_eq!(listed["tests"][0]["native_required"], true);

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    assert_eq!(report["status"], "failed");
    assert_eq!(report["passed"], 0);
    assert_eq!(report["failed"], 1);
    assert_eq!(report["unsupported"], 1);
    assert_eq!(report["native_skipped"], 0);
    assert_eq!(report["tests"][0]["status"], "unsupported");
    assert_eq!(report["tests"][0]["reference"]["status"], "passed");
    assert_eq!(report["tests"][0]["native"]["status"], "unsupported");
    assert_eq!(
        report["tests"][0]["native"]["reason_code"],
        "unsupported_feature"
    );
    assert_eq!(
        report["tests"][0]["native"]["diagnostics"][0]["kind"],
        "unsupported_feature"
    );
}

#[test]
fn native_result_json_distinguishes_skipped_and_native_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-result-statuses.sqlite");
    let source = temp.path().join("native-result-statuses.cdb");

    std::fs::write(
        &source,
        r#"
fn main() -> i64 = 3
fn inc(x: i64) -> i64 = x + 1
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    run(&[
        "create-test",
        path(&db),
        "inc_arg_native_skipped",
        "--entry",
        "inc",
        "--arg",
        "1",
        "--expect-i64",
        "2",
        "--native-agreement",
    ]);
    let skipped = parse_json(&run(&["test", path(&db), "--json"]));
    assert_eq!(skipped["status"], "passed");
    assert_eq!(skipped["native_skipped"], 1);
    assert_eq!(skipped["tests"][0]["status"], "passed");
    assert_eq!(skipped["tests"][0]["native"]["status"], "skipped");
    assert_eq!(
        skipped["tests"][0]["native"]["reason_code"],
        "unsupported_feature"
    );

    if !can_build_default_native_target() {
        return;
    }
    run(&[
        "create-test",
        path(&db),
        "main_native_mismatch",
        "--entry",
        "main",
        "--expect-i64",
        "4",
        "--native-agreement",
    ]);
    let mismatch = parse_json(&run(&["test", path(&db), "--json"]));
    let mismatch_test = mismatch["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|test| test["name"] == "main_native_mismatch")
        .unwrap();
    assert_eq!(mismatch["status"], "failed");
    assert_eq!(mismatch["native_mismatches"], 1);
    assert_eq!(mismatch_test["status"], "native_mismatch");
    assert_eq!(mismatch_test["reference"]["status"], "failed");
    assert_eq!(mismatch_test["native"]["status"], "native_mismatch");
    assert_eq!(mismatch_test["native"]["reason_code"], "native_mismatch");
    assert_eq!(
        mismatch_test["native"]["comparison"]["actual"],
        json!({"kind": "i64", "value": "3"})
    );
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
