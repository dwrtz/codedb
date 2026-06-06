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
fn type_move_add_and_remove_operations_update_type_definitions() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("type-ops.sqlite");
    let create = temp.path().join("create-types.json");
    let add_field = temp.path().join("add-field.json");
    let add_variant = temp.path().join("add-variant.json");
    let move_type = temp.path().join("move-type.json");
    let remove_field = temp.path().join("remove-field.json");
    let remove_variant = temp.path().join("remove-variant.json");

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
          { "name": "price_cents", "type": "i64" }
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
          { "name": "none", "type": "unit" }
        ]
      }
    }
  ]
}
"#,
    )
    .unwrap();
    run(&["apply", path(&db), "--json", path(&create)]);

    std::fs::write(
        &add_field,
        r#"{ "kind": "add_field", "type": "Line", "field": { "name": "qty", "type": "i64" } }"#,
    )
    .unwrap();
    std::fs::write(
        &add_variant,
        r#"{ "kind": "add_variant", "type": "Discount", "variant": { "name": "fixed", "type": "i64" } }"#,
    )
    .unwrap();
    std::fs::write(
        &move_type,
        r#"{ "kind": "move_type", "name": "Line", "new_module": "billing" }"#,
    )
    .unwrap();
    std::fs::write(
        &remove_field,
        r#"{ "kind": "remove_field", "module": "billing", "type": "Line", "field": "qty" }"#,
    )
    .unwrap();
    std::fs::write(
        &remove_variant,
        r#"{ "kind": "remove_variant", "type": "Discount", "variant": "fixed" }"#,
    )
    .unwrap();

    run(&["apply", path(&db), "--json", path(&add_field)]);
    run(&["apply", path(&db), "--json", path(&add_variant)]);
    run(&["apply", path(&db), "--json", path(&move_type)]);
    let moved = list_json(&db);
    let line = type_by_name(&moved, "Line");
    assert_eq!(line["module"], "billing");
    assert!(
        line["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| { field["name"].as_str() == Some("qty") })
    );
    assert!(
        type_by_name(&moved, "Discount")["variants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|variant| variant["name"].as_str() == Some("fixed"))
    );

    run(&["apply", path(&db), "--json", path(&remove_field)]);
    run(&["apply", path(&db), "--json", path(&remove_variant)]);
    run(&["verify", path(&db)]);
    let after = list_json(&db);
    assert!(
        !type_by_name(&after, "Line")["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field["name"].as_str() == Some("qty"))
    );
    assert!(
        !type_by_name(&after, "Discount")["variants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|variant| variant["name"].as_str() == Some("fixed"))
    );
}

#[test]
fn move_type_within_same_module_is_idempotent_no_op() {
    // A same-module move is a no-op. The apply path already short-circuits it,
    // but the MoveType postcondition previously asserted both
    // TypeNamePointsToType and a contradictory TypeNameAbsent for the same
    // module, so an idempotent move failed with a spurious postcondition error.
    // It must now succeed and leave the type unchanged.
    let temp = tempdir().unwrap();
    let db = temp.path().join("move-type-same-module.sqlite");
    let create = temp.path().join("create.json");
    let move_same = temp.path().join("move-same.json");

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
          { "name": "price_cents", "type": "i64" }
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
    let module_before = type_by_name(&before, "Line")["module"]
        .as_str()
        .unwrap()
        .to_string();

    std::fs::write(
        &move_same,
        format!(r#"{{ "kind": "move_type", "name": "Line", "new_module": "{module_before}" }}"#),
    )
    .unwrap();
    // Must not fail with a postcondition error.
    run(&["apply", path(&db), "--json", path(&move_same)]);
    run(&["verify", path(&db)]);

    let after = list_json(&db);
    assert_eq!(
        type_by_name(&after, "Line")["module"],
        module_before.as_str()
    );
}

#[test]
fn renamed_types_and_members_round_trip_projection_with_stable_identities() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("renamed-identities.sqlite");
    let rebuilt = temp.path().join("renamed-identities-rebuilt.sqlite");
    let source = temp.path().join("renamed-identities.cdb");
    let rename_type = temp.path().join("rename-type.json");
    let rename_field = temp.path().join("rename-field.json");
    let rename_variant = temp.path().join("rename-variant.json");
    let projection = temp.path().join("renamed-identities.projection.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

enum Discount {
  none: unit
  percent: i64
}

fn main() -> i64 = 1
"#,
    )
    .unwrap();
    std::fs::write(
        &rename_type,
        r#"{ "kind": "rename_type", "name": "Line", "new_name": "InvoiceLine" }"#,
    )
    .unwrap();
    std::fs::write(
        &rename_field,
        r#"{ "kind": "rename_field", "type": "InvoiceLine", "field": "price_cents", "new_name": "amount_cents" }"#,
    )
    .unwrap();
    std::fs::write(
        &rename_variant,
        r#"{ "kind": "rename_variant", "type": "Discount", "variant": "percent", "new_name": "pct" }"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["apply", path(&db), "--json", path(&rename_type)]);
    run(&["apply", path(&db), "--json", path(&rename_field)]);
    run(&["apply", path(&db), "--json", path(&rename_variant)]);
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
    assert!(exported.contains("record InvoiceLine"));
    assert!(exported.contains("amount_cents: i64"));
    assert!(exported.contains("pct: i64"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    run(&["verify", path(&rebuilt)]);

    assert_eq!(
        type_identity_summary(&list_json(&db)),
        type_identity_summary(&list_json(&rebuilt))
    );
}

#[test]
fn field_rename_updates_existing_function_bodies() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("rename-used-field.sqlite");
    let source = temp.path().join("rename-used-field.cdb");
    let rename_field = temp.path().join("rename-field.json");
    let projection = temp.path().join("renamed.projection.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn total(line: Line) -> i64 = line.price_cents * line.qty
fn main() -> i64 = total({ price_cents: 25, qty: 4 })
"#,
    )
    .unwrap();
    std::fs::write(
        &rename_field,
        r#"{ "kind": "rename_field", "type": "Line", "field": "price_cents", "new_name": "amount_cents" }"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");
    run(&["apply", path(&db), "--json", path(&rename_field)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");
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
    assert!(exported.contains("amount_cents: i64"));
    assert!(exported.contains("line.amount_cents * line.qty"));
    assert!(exported.contains("total({amount_cents: 25, qty: 4})"));
}

#[test]
fn variant_rename_updates_existing_function_bodies() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("rename-used-variant.sqlite");
    let source = temp.path().join("rename-used-variant.cdb");
    let rename_variant = temp.path().join("rename-variant.json");
    let projection = temp.path().join("renamed-variant.projection.cdb");

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
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("pct: i64"));
    assert!(exported.contains("pct(amount) => amount"));
    assert!(exported.contains("value(Discount::pct(10))"));
}

#[test]
fn distinct_named_records_with_same_shape_are_not_assignable() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("nominal-records.sqlite");
    let source = temp.path().join("nominal-records.cdb");

    std::fs::write(
        &source,
        r#"
record Cents {
  value: i64
}

record Quantity {
  value: i64
}

fn use_cents(c: Cents) -> i64 = c.value

fn main() -> i64 =
  let q: Quantity = { value: 7 } in
  use_cents(q)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "call arg 0 for use_cents expected",
        ))
        .stderr(predicate::str::contains("got type<"));
}

#[test]
fn named_record_cannot_be_erased_through_structural_record() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("nominal-record-erasure.sqlite");
    let source = temp.path().join("nominal-record-erasure.cdb");

    std::fs::write(
        &source,
        r#"
record Cents {
  value: i64
}

record Quantity {
  value: i64
}

fn erase(q: record { value: i64 }) -> record { value: i64 } = q

fn use_cents(c: Cents) -> i64 = c.value

fn main() -> i64 =
  let q: Quantity = { value: 7 } in
  use_cents(erase(q))
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("call arg 0 for erase expected"))
        .stderr(predicate::str::contains("got type<"));
}

#[test]
fn distinct_named_enums_with_same_shape_are_not_assignable() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("nominal-enums.sqlite");
    let source = temp.path().join("nominal-enums.cdb");

    std::fs::write(
        &source,
        r#"
enum PrimaryDiscount {
  none: unit
  percent: i64
}

enum SecondaryDiscount {
  none: unit
  percent: i64
}

fn value(discount: PrimaryDiscount) -> i64 =
  case discount of none => 0 | percent(amount) => amount

fn main() -> i64 = value(SecondaryDiscount::percent(10))
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("call arg 0 for value expected"))
        .stderr(predicate::str::contains("got type<"));
}

#[test]
fn named_enum_cannot_be_erased_through_structural_enum() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("nominal-enum-erasure.sqlite");
    let source = temp.path().join("nominal-enum-erasure.cdb");

    std::fs::write(
        &source,
        r#"
enum PrimaryDiscount {
  none: unit
  percent: i64
}

enum SecondaryDiscount {
  none: unit
  percent: i64
}

fn erase(discount: enum { none: unit, percent: i64 }) -> enum { none: unit, percent: i64 } =
  discount

fn value(discount: PrimaryDiscount) -> i64 =
  case discount of none => 0 | percent(amount) => amount

fn main() -> i64 = value(erase(SecondaryDiscount::percent(10)))
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("call arg 0 for erase expected"))
        .stderr(predicate::str::contains("got type<"));
}

#[test]
fn import_rejects_record_with_duplicate_field_names() {
    // Phase 3: duplicate record fields must be rejected (the projection parser
    // and the member validator both guard this).
    let temp = tempdir().unwrap();
    let db = temp.path().join("dup-field.sqlite");
    let source = temp.path().join("dup-field.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  price_cents: i64
}
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("duplicate record field"));
}

#[test]
fn import_rejects_enum_with_duplicate_variant_names() {
    // Phase 3: duplicate enum variants must be rejected.
    let temp = tempdir().unwrap();
    let db = temp.path().join("dup-variant.sqlite");
    let source = temp.path().join("dup-variant.cdb");

    std::fs::write(
        &source,
        r#"
enum Discount {
  none: unit
  none: i64
}
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("duplicate"));
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

type TypeIdentitySummary = Vec<(String, String, Vec<(String, String)>)>;

fn type_identity_summary(listing: &JsonValue) -> TypeIdentitySummary {
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

#[test]
fn diff_reports_type_and_member_changes() {
    // Regression: `diff` must surface type-definition changes (added types and
    // per-member field/variant changes keyed by stable member identity) rather
    // than reporting "Only root metadata or ordering changed".
    let temp = tempdir().unwrap();
    let db = temp.path().join("diff-types.sqlite");
    let create = temp.path().join("create.json");
    let rename = temp.path().join("rename.json");

    run(&["init", path(&db)]);
    let initial: JsonValue = serde_json::from_str(&run(&["list", path(&db), "--json"])).unwrap();
    let initial_root = initial["root_hash"].as_str().unwrap().to_string();

    std::fs::write(
        &create,
        r#"{
  "schema": "codedb/apply/v1",
  "operations": [
    {
      "kind": "create_type",
      "name": "Money",
      "birth_seed": "money-type",
      "definition": { "kind": "record", "fields": [ { "name": "cents", "type": "i64" } ] }
    }
  ]
}"#,
    )
    .unwrap();
    let created: JsonValue =
        serde_json::from_str(&run(&["apply", path(&db), "--json", path(&create)])).unwrap();
    let root_with_money = created["new_root_hash"].as_str().unwrap().to_string();

    // The create itself, diffed against the empty initial root, reports type_added.
    let create_diff = run(&["diff", path(&db), &initial_root, &root_with_money]);
    assert!(create_diff.contains("type_added"), "diff: {create_diff}");
    assert!(create_diff.contains("Money"), "diff: {create_diff}");

    std::fs::write(
        &rename,
        format!(
            r#"{{
  "schema": "codedb/apply/v1",
  "expect_root_hash": "{root_with_money}",
  "operations": [
    {{ "kind": "rename_field", "type": "Money", "field": "cents", "new_name": "pennies" }}
  ]
}}"#
        ),
    )
    .unwrap();
    let renamed: JsonValue =
        serde_json::from_str(&run(&["apply", path(&db), "--json", path(&rename)])).unwrap();
    let renamed_root = renamed["new_root_hash"].as_str().unwrap().to_string();

    // Text diff must show the field rename by identity, not a metadata-only note.
    let text = run(&["diff", path(&db), &root_with_money, &renamed_root]);
    assert!(text.contains("field_renamed"), "diff text: {text}");
    assert!(text.contains("pennies"), "diff text: {text}");
    assert!(
        !text.contains("Only root metadata or ordering changed"),
        "diff text falsely reported metadata-only: {text}"
    );

    // JSON diff must carry a field_renamed change record.
    let json: JsonValue =
        serde_json::from_str(&run(&["diff", path(&db), &root_with_money, &renamed_root, "--json"]))
            .unwrap();
    let kinds = json["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|change| change["kind"].as_str())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"field_renamed"), "kinds: {kinds:?}");
}

#[test]
fn reference_recursive_type_creates_verifies_and_lays_out() {
    // SPEC_V2 §11: a type may reference itself (and its region parameters)
    // through a reference field. Creation must succeed (two-phase resolution),
    // verification must pass, and the layout must place the self-reference as a
    // pointer-sized field rather than recursing forever.
    let temp = tempdir().unwrap();
    let db = temp.path().join("recursive.sqlite");
    let source = temp.path().join("recursive.cdb");
    let layout = temp.path().join("node.layout.json");

    std::fs::write(
        &source,
        r#"
record Node<'a> {
  value: i64
  next: &'a Node<'a>
}
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    run(&[
        "emit-type-layout",
        path(&db),
        "Node",
        "--out",
        path(&layout),
    ]);
    let layout: JsonValue =
        serde_json::from_str(&std::fs::read_to_string(&layout).unwrap()).unwrap();
    assert_eq!(layout["kind"], "record");
    let fields = layout["fields"].as_array().unwrap();
    let next = fields
        .iter()
        .find(|field| field["name"] == "next")
        .expect("next field");
    // The self-reference is a pointer-sized field (8 bytes), not the whole Node.
    assert_eq!(next["size_bytes"], 8);
}

#[test]
fn movable_self_referential_record_is_rejected_at_layout() {
    // A record that embeds itself by value (not through a reference) has no
    // finite layout. SPEC_V2 §22 lists this as a non-goal; it must be rejected
    // fail-closed at layout rather than recursing forever.
    let temp = tempdir().unwrap();
    let db = temp.path().join("movable-self.sqlite");
    let source = temp.path().join("movable-self.cdb");

    std::fs::write(
        &source,
        r#"
record Bad {
  next: Bad
}
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    bin()
        .args(["emit-type-layout", path(&db), "Bad", "--out", path(&db.with_extension("layout.json"))])
        .assert()
        .failure()
        .stderr(predicate::str::contains("recursive type layout is not supported"));
}
