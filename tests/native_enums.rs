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
fn native_enums_construct_case_dispatch_and_serialize_test_values() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-enums.sqlite");
    let source = temp.path().join("native-enums.cdb");
    let apply = temp.path().join("native-enums.apply.json");
    let amount_ir_path = temp.path().join("amount.ir.json");
    let make_percent_object = temp.path().join("make-percent.o");

    std::fs::write(
        &source,
        r#"
enum Discount {
  none: unit
  percent: i64
  fixed: i64
}

fn amount(discount: Discount, subtotal: i64) -> i64 =
  case discount of none => subtotal | percent(pct) => subtotal - subtotal * pct / 100 | fixed(cents) => subtotal - cents

fn make_percent() -> Discount = Discount::percent(20)

fn choose(flag: bool) -> Discount =
  if flag then Discount::fixed(150) else Discount::none

fn percent_amount() -> i64 = amount(make_percent(), 1000)
fn fixed_amount() -> i64 = amount(choose(true), 1000)
fn none_amount() -> i64 = amount(choose(false), 1000)

fn main() -> i64 = percent_amount() + fixed_amount() + none_amount()
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "2650");
    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let case_variants = trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "case_decision")
        .map(|event| event["selected_variant"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(case_variants.contains(&"percent"));
    assert!(case_variants.contains(&"fixed"));
    assert!(case_variants.contains(&"none"));
    assert!(trace["events"].as_array().unwrap().iter().any(|event| {
        event["event"] == "case_decision"
            && event["expr_hash"].as_str().is_some()
            && event["selected_expr_hash"].as_str().is_some()
    }));

    run(&[
        "emit-ir",
        path(&db),
        "amount",
        "--out",
        path(&amount_ir_path),
    ]);
    let amount_ir = read_json(&amount_ir_path);
    let operations = amount_ir["ir"]["operations"].as_array().unwrap();
    assert!(operations.iter().any(|op| op["op"] == "case"));
    let case_op = operations.iter().find(|op| op["op"] == "case").unwrap();
    assert_eq!(case_op["arms"].as_array().unwrap().len(), 3);
    assert!(
        amount_ir["ir"]["debug_map"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["lowered_op_kind"] == "case")
    );

    run(&[
        "emit-object",
        path(&db),
        "make_percent",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&make_percent_object),
    ]);
    run(&["verify", path(&db)]);

    std::fs::write(
        &apply,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "operations": [
                {
                    "kind": "create_test",
                    "name": "main_native_enum_case",
                    "entry": "main",
                    "native_required": true,
                    "expected": { "kind": "i64", "value": "2650" }
                },
                {
                    "kind": "create_test",
                    "name": "enum_return_native",
                    "entry": "make_percent",
                    "native_required": true,
                    "expected": {
                        "kind": "enum",
                        "variant": "percent",
                        "value": { "kind": "i64", "value": "20" }
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let applied = parse_json(&run(&["apply", path(&db), "--json", path(&apply)]));
    assert_eq!(applied["status"], "applied");

    let listed = parse_json(&run(&["test", path(&db), "--list", "--json"]));
    let listed_enum = listed["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|test| test["name"] == "enum_return_native")
        .unwrap();
    assert_eq!(listed_enum["expected"]["kind"], "enum");
    assert_eq!(listed_enum["expected"]["variant"], "percent");

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["passed"], 2);
        assert_eq!(report["unsupported"], 0);
        let enum_test = report["tests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|test| test["name"] == "enum_return_native")
            .unwrap();
        assert_eq!(enum_test["reference"]["actual"]["kind"], "enum");
        assert_eq!(enum_test["native"]["status"], "passed");
        assert_eq!(
            enum_test["native"]["comparison"]["kind"],
            "native_aggregate_harness"
        );
        assert_eq!(enum_test["native"]["comparison"]["actual"]["kind"], "enum");
    } else {
        assert_eq!(report["status"], "failed");
        assert_eq!(report["unsupported"], 2);
    }
}

#[test]
fn non_exhaustive_case_is_rejected() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("non-exhaustive-enum.sqlite");
    let source = temp.path().join("non-exhaustive-enum.cdb");

    std::fs::write(
        &source,
        r#"
enum Sel {
  yes: i64
  no: unit
}

fn bad(sel: Sel) -> i64 =
  case sel of yes(value) => value
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "case expression must cover every enum variant",
        ));
}

#[test]
fn renamed_enum_variant_still_lowers_to_native_case() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("renamed-native-enum.sqlite");
    let source = temp.path().join("renamed-native-enum.cdb");
    let rename_variant = temp.path().join("rename-variant.json");
    let main_object = temp.path().join("main.o");

    std::fs::write(
        &source,
        r#"
enum Discount {
  none: unit
  percent: i64
}

fn amount(discount: Discount) -> i64 =
  case discount of none => 0 | percent(value) => value

fn main() -> i64 = amount(Discount::percent(10))
"#,
    )
    .unwrap();
    std::fs::write(
        &rename_variant,
        r#"{ "kind": "rename_variant", "type": "Discount", "variant": "percent", "new_name": "pct" }"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "10");
    run(&["apply", path(&db), "--json", path(&rename_variant)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "10");
    run(&["verify", path(&db)]);
    run(&[
        "emit-object",
        path(&db),
        "main",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&main_object),
    ]);
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
