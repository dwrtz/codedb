use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn current_root(db: &Path) -> String {
    parse_json(&run(&["list", path(db), "--json"]))["root_hash"]
        .as_str()
        .expect("root hash")
        .to_string()
}

fn branch_state(db: &Path) -> (String, Option<String>) {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT root_hash, history_hash FROM branches WHERE name = 'main'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn write_patch(dir: &Path, name: &str, patch: JsonValue) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(&patch).unwrap()).unwrap();
    path
}

fn list_json(db: &Path) -> JsonValue {
    parse_json(&run(&["list", path(db), "--json"]))
}

fn type_by_name<'a>(listing: &'a JsonValue, name: &str) -> &'a JsonValue {
    listing["types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"].as_str() == Some(name))
        .unwrap()
}

fn member_symbol(type_entry: &JsonValue, member_key: &str, name: &str) -> String {
    type_entry[member_key]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"].as_str() == Some(name))
        .unwrap()["symbol_hash"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn semantic_patch_rename_field_preserves_identity_and_reports_v2_impact() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("v2-rename-field.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/v2/line_view_refs.cdb"]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");

    let before = list_json(&db);
    let price_symbol = member_symbol(type_by_name(&before, "Line"), "fields", "price_cents");
    let root = current_root(&db);
    let patch = write_patch(
        temp.path(),
        "rename-field.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "type",
                "name": "Line"
            },
            "replace": {
                "kind": "rename_field",
                "field": "price_cents",
                "new_name": "amount_cents"
            }
        }),
    );

    let preview = parse_json(&run(&[
        "patch",
        "preview",
        path(&db),
        "--json",
        path(&patch),
    ]));
    assert_eq!(preview["status"], "planned");
    assert_eq!(preview["planned_operations"][0]["kind"], "rename_field");
    assert_eq!(
        preview["v2_impact"]["layout_impact"],
        json!(["identity_preserved_no_layout_change"])
    );
    assert_eq!(
        preview["v2_impact"]["codegen_impact"],
        json!(["semantic_member_rewrite"])
    );

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "applied");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");
    run(&["verify", path(&db)]);

    let after = list_json(&db);
    assert_eq!(
        member_symbol(type_by_name(&after, "Line"), "fields", "amount_cents"),
        price_symbol
    );
    let shown = parse_json(&run(&["show", path(&db), "line_total", "--json"]));
    assert!(
        shown["body_source"]
            .as_str()
            .unwrap()
            .contains("amount_cents")
    );
}

#[test]
fn semantic_patch_rename_variant_updates_constructors_and_cases() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("v2-rename-variant.sqlite");
    let source = temp.path().join("discount.cdb");
    std::fs::write(
        &source,
        r#"
enum Discount {
  none: unit
  percent: i64
}

fn value(discount: Discount) -> i64 =
  case discount of none => 0 | percent(amount) => amount

fn main() -> i64 = value(Discount::percent(10))
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "10");

    let before = list_json(&db);
    let percent_symbol = member_symbol(type_by_name(&before, "Discount"), "variants", "percent");
    let root = current_root(&db);
    let patch = write_patch(
        temp.path(),
        "rename-variant.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "type",
                "name": "Discount"
            },
            "replace": {
                "kind": "rename_variant_and_cases",
                "variant": "percent",
                "new_name": "pct"
            }
        }),
    );

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        applied["semantic_summary"]["operation_kinds"],
        json!(["rename_variant"])
    );
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "10");
    run(&["verify", path(&db)]);

    let after = list_json(&db);
    assert_eq!(
        member_symbol(type_by_name(&after, "Discount"), "variants", "pct"),
        percent_symbol
    );
    let show_main = parse_json(&run(&["show", path(&db), "main", "--json"]));
    assert_eq!(show_main["body_source"], "value(Discount::pct(10))");
    let show_value = parse_json(&run(&["show", path(&db), "value", "--json"]));
    assert!(
        show_value["body_source"]
            .as_str()
            .unwrap()
            .contains("pct(amount)")
    );
}

#[test]
fn semantic_patch_convert_by_value_param_to_ref_updates_callers_and_native_tests() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("v2-convert-param.sqlite");
    let source = temp.path().join("line-total.cdb");
    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn line_total<'a>(line: Line) -> i64 =
  line.price_cents * line.qty

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  line_total(line)
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");

    let root = current_root(&db);
    let patch = write_patch(
        temp.path(),
        "convert-param.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "symbol",
                "name": "line_total"
            },
            "replace": {
                "kind": "convert_by_value_param_to_ref",
                "param": "line",
                "region": "a"
            }
        }),
    );

    let preview = parse_json(&run(&[
        "patch",
        "preview",
        path(&db),
        "--json",
        path(&patch),
    ]));
    assert_eq!(preview["status"], "planned");
    assert_eq!(
        preview["planned_operations"][0]["kind"],
        "convert_param_to_reference"
    );
    assert_eq!(
        preview["v2_impact"]["region_impact"],
        json!(["region_parameter_required"])
    );
    assert_eq!(
        preview["v2_impact"]["borrow_impact"],
        json!(["shared_borrow_introduced"])
    );
    assert_eq!(
        preview["v2_impact"]["layout_impact"],
        json!(["signature_abi_changed_to_reference"])
    );

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "applied");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");
    run(&["verify", path(&db)]);

    let total = parse_json(&run(&["show", path(&db), "line_total", "--json"]));
    assert_eq!(total["signature"], "<'a>(line: &'a Line) -> i64");
    let main = parse_json(&run(&["show", path(&db), "main", "--json"]));
    assert!(
        main["body_source"]
            .as_str()
            .unwrap()
            .contains("line_total(&'a line)")
    );

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "convert_param_ref_native",
            "--entry",
            "main",
            "--expect-i64",
            "100",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
    }
}

#[test]
fn failed_convert_by_value_param_to_ref_leaves_branch_unchanged() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("v2-convert-param-fails.sqlite");
    let source = temp.path().join("line-total-temporary.cdb");
    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn make_line() -> Line = { price_cents: 25, qty: 4 }

fn line_total<'a>(line: Line) -> i64 =
  line.price_cents * line.qty

fn main<'a>() -> i64 = line_total(make_line())
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    let before = branch_state(&db);
    let root = before.0.clone();
    let patch = write_patch(
        temp.path(),
        "convert-temporary.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "symbol",
                "name": "line_total"
            },
            "replace": {
                "kind": "convert_by_value_param_to_ref",
                "param": "line",
                "region": "a"
            }
        }),
    );

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "error");
    assert_eq!(applied["committed"], false);
    assert_eq!(branch_state(&db), before);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");
}

fn can_build_default_native_target() -> bool {
    let native_target = matches!(
        codedb::DEFAULT_NATIVE_TARGET,
        codedb::LINUX_X86_64_TARGET | codedb::APPLE_ARM64_TARGET
    );
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
