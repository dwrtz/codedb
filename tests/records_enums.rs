use std::path::Path;

use assert_cmd::Command;
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn record_values_type_check_evaluate_verify_and_round_trip_projection() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("records.sqlite");
    let source = temp.path().join("records.cdb");
    let projection = temp.path().join("records.projection.cdb");
    let rebuilt = temp.path().join("records-rebuilt.sqlite");

    std::fs::write(
        &source,
        r#"
fn add_tax(order: record { amount: i64, tax: i64 }) -> i64 = order.amount + order.tax
fn main() -> i64 = add_tax({ amount: 100, tax: 20 })
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");
    run(&["verify", path(&db)]);

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("record {amount: i64, tax: i64}"));
    assert!(exported.contains("order.amount + order.tax"));
    assert!(exported.contains("{amount: 100, tax: 20}"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "120");
    run(&["verify", path(&rebuilt)]);

    let main_ir = temp.path().join("main.ir.json");
    run(&["emit-ir", path(&db), "main", "--out", path(&main_ir)]);
    let lowered = std::fs::read_to_string(&main_ir).unwrap();
    assert!(lowered.contains("\"record\""));
    assert!(lowered.contains("\"by_indirect\""));
}

#[test]
fn enum_values_type_check_evaluate_verify_and_round_trip_projection() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("enums.sqlite");
    let source = temp.path().join("enums.cdb");
    let projection = temp.path().join("enums.projection.cdb");
    let rebuilt = temp.path().join("enums-rebuilt.sqlite");

    std::fs::write(
        &source,
        r#"
fn maybe_value() -> enum { none: (), some: i64 } = enum { none: (), some: i64 }::some(41)
fn main() -> i64 = case maybe_value() of none => 0 | some(x) => x + 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "42");
    run(&["verify", path(&db)]);

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("enum {none: unit, some: i64}::some(41)"));
    assert!(exported.contains("case maybe_value() of none => 0 | some(x) => x + 1"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "42");
    run(&["verify", path(&rebuilt)]);

    run(&[
        "emit-object",
        path(&db),
        "maybe_value",
        "--out",
        path(&temp.path().join("maybe.o")),
    ]);
    run(&["verify", path(&db)]);
}
