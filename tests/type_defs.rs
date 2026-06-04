use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
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
    path.to_str().unwrap()
}

#[test]
fn named_records_enums_and_region_params_round_trip_with_stable_identities() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("types.sqlite");
    let source = temp.path().join("types.cdb");
    let projection = temp.path().join("types.projection.cdb");
    let rebuilt = temp.path().join("types-rebuilt.sqlite");

    std::fs::write(
        &source,
        r#"
record Money {
  cents: i64
}

enum Discount {
  none: unit
  percent: i64
  fixed: Money
}

record Line {
  price: Money
  qty: i64
}

record LineView<'a> {
  line: Line
}

fn passthrough(m: Money) -> Money = m
fn main() -> i64 = 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
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
    assert!(exported.contains("record Money"));
    assert!(exported.contains("record LineView<'a>"));
    assert!(exported.contains("line: Line"));
    assert!(exported.contains("enum Discount"));
    assert!(exported.contains("fn passthrough(m: Money) -> Money = m"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    run(&["verify", path(&rebuilt)]);

    let original = list_json(&db);
    let round_tripped = list_json(&rebuilt);
    assert_eq!(
        type_identity_summary(&original),
        type_identity_summary(&round_tripped)
    );

    let conn = Connection::open(&db).unwrap();
    let root_hash = original["root_hash"].as_str().unwrap();
    let type_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM root_types WHERE root_hash = ?1",
            [root_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(type_count, 4);
}

#[test]
fn field_and_variant_renames_preserve_member_identity() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("migrations.sqlite");
    let create = temp.path().join("create-types.json");
    let rename_field = temp.path().join("rename-field.json");
    let rename_variant = temp.path().join("rename-variant.json");

    run(&["init", path(&db)]);
    std::fs::write(
        &create,
        r#"
{
  "schema": "codedb/apply/v1",
  "operations": [
    {
      "kind": "create_type",
      "name": "Line",
      "birth_seed": "line-type",
      "definition": {
        "kind": "record",
        "fields": [
          { "name": "price_cents", "type": "i64" },
          { "name": "qty", "type": "i64" }
        ]
      }
    },
    {
      "kind": "create_type",
      "name": "Discount",
      "birth_seed": "discount-type",
      "definition": {
        "kind": "enum",
        "variants": [
          { "name": "none", "type": "unit" },
          { "name": "percent", "type": "i64" }
        ]
      }
    }
  ]
}
"#,
    )
    .unwrap();
    run(&["apply", path(&db), "--json", path(&create)]);

    let before = list_json(&db);
    let line = type_by_name(&before, "Line");
    let price_symbol = member_symbol(line, "fields", "price_cents");
    let discount = type_by_name(&before, "Discount");
    let percent_symbol = member_symbol(discount, "variants", "percent");

    std::fs::write(
        &rename_field,
        r#"{ "kind": "rename_field", "type": "Line", "field": "price_cents", "new_name": "amount_cents" }"#,
    )
    .unwrap();
    run(&["apply", path(&db), "--json", path(&rename_field)]);

    std::fs::write(
        &rename_variant,
        r#"{ "kind": "rename_variant", "type": "Discount", "variant": "percent", "new_name": "pct" }"#,
    )
    .unwrap();
    run(&["apply", path(&db), "--json", path(&rename_variant)]);
    run(&["verify", path(&db)]);

    let after = list_json(&db);
    assert_eq!(
        member_symbol(type_by_name(&after, "Line"), "fields", "amount_cents"),
        price_symbol
    );
    assert_eq!(
        member_symbol(type_by_name(&after, "Discount"), "variants", "pct"),
        percent_symbol
    );
}

#[test]
fn verify_rejects_type_definition_with_invalid_region_reference() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-region.sqlite");
    let source = temp.path().join("regions.cdb");

    std::fs::write(
        &source,
        r#"
record Box<'a> {
  value: i64
}

record Holder<'h> {
  boxed: Box<'h>
}
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);

    let listing = list_json(&db);
    let box_region = type_by_name(&listing, "Box")["region_params"][0]["region_hash"]
        .as_str()
        .unwrap();
    let holder = type_by_name(&listing, "Holder");
    let field_type_hash = holder["fields"][0]["type_hash"].as_str().unwrap();
    let boxed_type_symbol = type_by_name(&listing, "Box")["type_symbol_hash"]
        .as_str()
        .unwrap();

    let corrupt_payload = format!(
        r#"{{"region_args":["{box_region}"],"type_kind":"Named","type_symbol":"{boxed_type_symbol}"}}"#
    );
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE objects SET payload_json = ?1 WHERE hash = ?2",
        (&corrupt_payload, field_type_hash),
    )
    .unwrap();

    bin()
        .args(["verify", path(&db)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid region reference"));
}

fn list_json(db: &Path) -> JsonValue {
    serde_json::from_str(&run(&["list", path(db), "--json"])).unwrap()
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

fn type_identity_summary(listing: &JsonValue) -> Vec<(String, String, Vec<(String, String)>)> {
    listing["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let member_key = if entry.get("fields").is_some() {
                "fields"
            } else {
                "variants"
            };
            let members = entry[member_key]
                .as_array()
                .unwrap()
                .iter()
                .map(|member| {
                    (
                        member["name"].as_str().unwrap().to_string(),
                        member["symbol_hash"].as_str().unwrap().to_string(),
                    )
                })
                .collect::<Vec<_>>();
            (
                entry["name"].as_str().unwrap().to_string(),
                entry["type_symbol_hash"].as_str().unwrap().to_string(),
                members,
            )
        })
        .collect()
}
