use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
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

fn read_json(path: &Path) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn fold_over_fixed_array_and_slice_lowers_traces_exports_and_runs_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("loops.sqlite");
    let rebuilt = temp.path().join("loops-rebuilt.sqlite");
    let source = temp.path().join("loops.cdb");
    let projection = temp.path().join("loops.projection.cdb");
    let array_ir_path = temp.path().join("sum-array.ir.json");
    let slice_ir_path = temp.path().join("sum-slice-main.ir.json");

    std::fs::write(
        &source,
        r#"
fn sum_array() -> i64 =
  let values: array<i64, 4> = [2, 4, 6, 8] in
  fold value in values with total = 0 do total + value

fn sum_slice<'a>(s: slice<'a, i64>) -> i64 =
  fold value in s with total = 0 do total + value

fn main<'a>() -> i64 =
  let values: array<i64, 4> = [1, 3, 5, 7] in
  let s: slice<'a, i64> = slice(values) in
  sum_array() + sum_slice(s)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "36");
    run(&["verify", path(&db)]);

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "36"}));
    let loop_iterations = trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "loop_iteration")
        .collect::<Vec<_>>();
    assert_eq!(loop_iterations.len(), 8);
    assert_eq!(loop_iterations[0]["iteration"], 0);
    assert_eq!(
        loop_iterations[0]["accumulator_after"],
        json!({"kind": "i64", "value": "2"})
    );

    run(&[
        "emit-ir",
        path(&db),
        "sum_array",
        "--out",
        path(&array_ir_path),
    ]);
    run(&[
        "emit-ir",
        path(&db),
        "sum_slice",
        "--out",
        path(&slice_ir_path),
    ]);
    let array_ir = read_json(&array_ir_path);
    assert!(op_names(&array_ir).contains(&"fold".to_string()));
    assert!(debug_kinds(&array_ir).contains(&"fold".to_string()));
    let fold = array_ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["op"] == "fold")
        .expect("fold op");
    assert_eq!(fold["element_type_hash"], fold["acc_type_hash"]);
    let slice_ir = read_json(&slice_ir_path);
    assert!(op_names(&slice_ir).contains(&"fold".to_string()));

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("fold value in values with total = 0 do total + value"));
    assert!(exported.contains("fold value in s with total = 0 do total + value"));
    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "36");
    run(&["verify", path(&rebuilt)]);

    if can_build_default_native_target() {
        run(&[
            "create-test",
            path(&db),
            "loops_native",
            "--entry",
            "main",
            "--expect-i64",
            "36",
            "--native-required",
            "--json",
        ]);
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    }
}

#[test]
fn invoice_static_fixture_uses_slice_fold_records_and_enums_natively() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("invoice.sqlite");
    let ir_path = temp.path().join("invoice.ir.json");
    let source = Path::new("examples/v2/invoice_static.cdb");

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "145");
    run(&["verify", path(&db)]);
    run(&[
        "emit-ir",
        path(&db),
        "invoice_total",
        "--out",
        path(&ir_path),
    ]);
    let ir = read_json(&ir_path);
    assert!(op_names(&ir).contains(&"fold".to_string()));

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "145"}));
    assert_eq!(
        trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == "loop_iteration")
            .count(),
        3
    );

    if can_build_default_native_target() {
        run(&[
            "create-test",
            path(&db),
            "invoice_native",
            "--entry",
            "main",
            "--expect-i64",
            "145",
            "--native-required",
            "--json",
        ]);
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    }
}

#[test]
fn fold_body_mutation_requires_state_effect() {
    let temp = tempdir().unwrap();
    let good_db = temp.path().join("fold-state.sqlite");
    let good_source = temp.path().join("fold-state.cdb");
    let bad_db = temp.path().join("fold-state-bad.sqlite");
    let bad_source = temp.path().join("fold-state-bad.cdb");

    let program = r#"
fn mutate_sum<'a>() -> i64 effects[state] =
  let values: array<i64, 2> = [1, 2] in
  let s: mut_slice<'a, i64> = mut_slice(values) in
  fold value in s with total = 0 do
    let _: unit = s[0] = value in
    total + value
"#;
    std::fs::write(&good_source, program).unwrap();
    run(&["init", path(&good_db)]);
    run(&["import", path(&good_db), path(&good_source)]);
    assert_eq!(run(&["eval", path(&good_db), "mutate_sum"]).trim(), "3");
    run(&["verify", path(&good_db)]);

    // A fold body that mutates through a mutable slice must compile to native
    // code and agree with the oracle, and the function must surface the `state`
    // effect (loop-body mutation is a real store, not just an eval-time effect).
    if can_build_default_native_target() {
        run(&[
            "create-test",
            path(&good_db),
            "fold_mutation_native",
            "--entry",
            "mutate_sum",
            "--expect-i64",
            "3",
            "--native-required",
            "--json",
        ]);
        let report = parse_json(&run(&["test", path(&good_db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["entry_effects"], json!(["state"]));
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "3"})
        );
    }

    std::fs::write(&bad_source, program.replace(" effects[state]", "")).unwrap();
    run(&["init", path(&bad_db)]);
    bin()
        .args(["import", path(&bad_db), path(&bad_source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_effects"))
        .stderr(predicate::str::contains("state"));
}

#[test]
fn fold_over_array_conflicts_with_live_mut_slice_loan() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("fold-alias.sqlite");
    let source = temp.path().join("fold-alias.cdb");

    std::fs::write(
        &source,
        r#"
fn bad<'a>() -> i64 =
  let values: array<i64, 2> = [1, 2] in
  let s: mut_slice<'a, i64> = mut_slice(values) in
  fold value in values with total = 0 do total + value
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_borrow"))
        .stderr(predicate::str::contains("shared read"))
        .stderr(predicate::str::contains("live mutable borrow"));
}

fn op_names(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect()
}

fn debug_kinds(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["debug_map"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["lowered_op_kind"].as_str().unwrap().to_string())
        .collect()
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
