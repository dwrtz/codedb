use std::path::Path;
use std::process::Command as StdCommand;

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

fn run_failure(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
}

fn parse_line_value<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .unwrap_or_else(|| panic!("missing line prefix {key:?} in:\n{text}"))
        .trim()
}

#[test]
fn shop_demo_flow_preserves_symbol_identity_across_rename() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("demo.sqlite");
    let projection = temp.path().join("projection.cdb");
    let c_file = temp.path().join("projection.c");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    bin()
        .args(["eval", db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("120\n");

    bin()
        .args(["callers", db.to_str().unwrap(), "tax"])
        .assert()
        .success()
        .stdout(predicate::str::contains("total"));

    let rename = run(&["rename", db.to_str().unwrap(), "tax", "vat"]);
    let old_root = parse_line_value(&rename, "old_root ");
    let new_root = parse_line_value(&rename, "new_root ");
    assert!(rename.contains("build_impact metadata_only"));
    assert!(rename.contains("recompile none"));
    assert!(rename.contains("relink false"));

    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root]);
    assert!(diff.contains("symbol_renamed"));
    assert!(diff.contains("main.tax -> main.vat"));
    assert!(diff.contains("function body hash: unchanged"));
    assert!(diff.contains("Incremental build impact"));
    assert!(diff.contains("build_impact metadata_only"));

    let branch_before_retry = branch_state(&db);
    let migrations_before_retry = row_count(&db, "migrations");
    let retry = run(&["rename", db.to_str().unwrap(), "tax", "vat"]);
    assert!(retry.contains("already_applied rename_symbol main.tax -> main.vat"));
    assert_eq!(branch_state(&db), branch_before_retry);
    assert_eq!(row_count(&db, "migrations"), migrations_before_retry);

    run(&[
        "export",
        db.to_str().unwrap(),
        "--branch",
        "main",
        "--out",
        projection.to_str().unwrap(),
    ]);
    let source = std::fs::read_to_string(&projection).unwrap();
    assert!(source.contains("fn vat(subtotal: i64) -> i64"));
    assert!(source.contains("subtotal + vat(subtotal)"));
    assert!(!source.contains("tax("));

    run(&[
        "emit-c",
        db.to_str().unwrap(),
        "main",
        "--out",
        c_file.to_str().unwrap(),
    ]);
    let c_source = std::fs::read_to_string(&c_file).unwrap();
    assert!(c_source.contains("long codedb_vat(long subtotal)"));
    assert!(c_source.contains("return subtotal + codedb_vat(subtotal);"));
    for forbidden in ["malloc", "free", "printf", "pthread_"] {
        assert!(!c_source.contains(forbidden));
    }

    let cache_rows = cache_rows(&db);
    assert!(cache_rows.contains(&(
        "projection".to_string(),
        "canonical_source".to_string(),
        "canonical_source".to_string()
    )));
    assert!(cache_rows.contains(&(
        "projection".to_string(),
        "c_source".to_string(),
        "c_projection".to_string()
    )));
    assert!(
        cache_rows
            .iter()
            .all(|(_, _, artifact_kind)| artifact_kind != "rendered_source")
    );

    compile_and_run_c_if_available(&temp.path().join("projection.c"));

    bin()
        .args(["replay", db.to_str().unwrap(), "--from-genesis"])
        .assert()
        .success()
        .stdout(predicate::str::contains("replay ok"));

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn replace_body_updates_only_implementation_and_literal_diff() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("replace.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let replace = run(&[
        "replace-body",
        db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    let old_root = parse_line_value(&replace, "old_root ");
    let new_root = parse_line_value(&replace, "new_root ");
    assert!(replace.contains("build_impact recompile_symbols"));
    assert!(replace.contains("relink true"));
    let recompile = parse_line_value(&replace, "recompile ");
    assert!(recompile.starts_with("sha256:"));
    assert!(!recompile.contains(','));

    bin()
        .args(["eval", db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("118\n");

    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root]);
    assert!(diff.contains("implementation_changed"));
    assert!(diff.contains("signature: unchanged"));
    assert!(diff.contains("literal_changed: 20 -> 18"));
    assert!(diff.contains("compile impact: recompile_symbols"));

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn build_impact_is_available_as_json() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("json.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let rename = run(&["rename", db.to_str().unwrap(), "tax", "vat", "--json"]);
    let value: JsonValue = serde_json::from_str(&rename).unwrap();

    assert_eq!(value["status"], "applied");
    assert_eq!(value["summary"]["build_impact"]["kind"], "metadata_only");
    assert_eq!(value["summary"]["build_impact"]["relink"], false);
    assert_eq!(
        value["summary"]["build_impact"]["recompile"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let old_root = value["old_root_hash"].as_str().unwrap();
    let new_root = value["new_root_hash"].as_str().unwrap();
    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root, "--json"]);
    let diff: JsonValue = serde_json::from_str(&diff).unwrap();
    assert_eq!(diff["build_impact"]["kind"], "metadata_only");
    assert_eq!(diff["changes"][0]["kind"], "symbol_renamed");
}

#[test]
fn conditionals_and_booleans_import_and_evaluate() {
    let temp = tempdir().unwrap();

    let discount_db = temp.path().join("discount.sqlite");
    run(&["init", discount_db.to_str().unwrap()]);
    run(&[
        "import",
        discount_db.to_str().unwrap(),
        "examples/discount.cdb",
    ]);
    bin()
        .args(["eval", discount_db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("165\n");
    bin()
        .args(["verify", discount_db.to_str().unwrap()])
        .assert()
        .success();

    let bool_db = temp.path().join("booleans.sqlite");
    run(&["init", bool_db.to_str().unwrap()]);
    run(&["import", bool_db.to_str().unwrap(), "examples/booleans.cdb"]);
    bin()
        .args(["eval", bool_db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("5\n");
    bin()
        .args(["verify", bool_db.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn stale_expected_root_returns_conflict_without_writes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("conflict.sqlite");

    run(&["init", db.to_str().unwrap()]);
    let import = run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let expected_root = parse_line_value(&import, "root ");

    run(&["create-alias", db.to_str().unwrap(), "tax", "sales_tax"]);
    let branch_before_conflict = branch_state(&db);
    let counts_before_conflict = mutation_guard_counts(&db);

    let conflict = run(&[
        "rename",
        db.to_str().unwrap(),
        "tax",
        "vat",
        "--expect-root",
        expected_root,
    ]);
    assert!(conflict.contains("conflict rename_symbol main.tax -> main.vat"));
    assert!(conflict.contains(&format!("expected_root {expected_root}")));
    assert!(conflict.contains("failed_preconditions root_is_current"));
    assert_eq!(branch_state(&db), branch_before_conflict);
    assert_eq!(mutation_guard_counts(&db), counts_before_conflict);

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn structural_operations_retry_with_expected_root_return_already_applied() {
    let temp = tempdir().unwrap();

    let import_db = temp.path().join("import.sqlite");
    run(&["init", import_db.to_str().unwrap()]);
    run(&["import", import_db.to_str().unwrap(), "examples/shop.cdb"]);
    let branch_before_import_retry = branch_state(&import_db);
    let counts_before_import_retry = mutation_guard_counts(&import_db);
    let import_retry = run(&["import", import_db.to_str().unwrap(), "examples/shop.cdb"]);
    assert!(import_retry.contains("already_applied create_function main.tax"));
    assert!(import_retry.contains("already_applied create_function main.total"));
    assert!(import_retry.contains("already_applied create_function main.main"));
    assert_eq!(branch_state(&import_db), branch_before_import_retry);
    assert_eq!(
        mutation_guard_counts(&import_db),
        counts_before_import_retry
    );

    let replace_db = temp.path().join("replace-retry.sqlite");
    run(&["init", replace_db.to_str().unwrap()]);
    run(&["import", replace_db.to_str().unwrap(), "examples/shop.cdb"]);
    let replace = run(&[
        "replace-body",
        replace_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    let replace_expected_root = parse_line_value(&replace, "old_root ");
    assert!(replace.contains("build_impact recompile_symbols"));
    let branch_before_replace_retry = branch_state(&replace_db);
    let counts_before_replace_retry = mutation_guard_counts(&replace_db);
    let replace_retry = run(&[
        "replace-body",
        replace_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
        "--expect-root",
        replace_expected_root,
    ]);
    assert!(replace_retry.contains("already_applied replace_function_body main.tax"));
    assert_eq!(branch_state(&replace_db), branch_before_replace_retry);
    assert_eq!(
        mutation_guard_counts(&replace_db),
        counts_before_replace_retry
    );

    let signature_db = temp.path().join("signature-retry.sqlite");
    let signature_source = temp.path().join("signature.cdb");
    std::fs::write(
        &signature_source,
        "fn ignore(x: i64) -> i64 = 1\n\nfn main() -> i64 = ignore(5)\n",
    )
    .unwrap();
    run(&["init", signature_db.to_str().unwrap()]);
    run(&[
        "import",
        signature_db.to_str().unwrap(),
        signature_source.to_str().unwrap(),
    ]);
    let signature = run(&[
        "change-signature",
        signature_db.to_str().unwrap(),
        "ignore",
        "(y: i64) -> i64",
    ]);
    let signature_expected_root = parse_line_value(&signature, "old_root ");
    assert!(signature.contains("build_impact metadata_only"));
    let branch_before_signature_retry = branch_state(&signature_db);
    let counts_before_signature_retry = mutation_guard_counts(&signature_db);
    let signature_retry = run(&[
        "change-signature",
        signature_db.to_str().unwrap(),
        "ignore",
        "(y: i64) -> i64",
        "--expect-root",
        signature_expected_root,
    ]);
    assert!(signature_retry.contains("already_applied change_function_signature main.ignore"));
    assert_eq!(branch_state(&signature_db), branch_before_signature_retry);
    assert_eq!(
        mutation_guard_counts(&signature_db),
        counts_before_signature_retry
    );

    let delete_db = temp.path().join("delete-retry.sqlite");
    let delete_source = temp.path().join("delete.cdb");
    std::fs::write(
        &delete_source,
        "fn unused() -> i64 = 1\n\nfn main() -> i64 = 2\n",
    )
    .unwrap();
    run(&["init", delete_db.to_str().unwrap()]);
    run(&[
        "import",
        delete_db.to_str().unwrap(),
        delete_source.to_str().unwrap(),
    ]);
    let delete = run(&["delete-symbol", delete_db.to_str().unwrap(), "unused"]);
    let delete_expected_root = parse_line_value(&delete, "old_root ");
    assert!(delete.contains("build_impact relink_only"));
    let branch_before_delete_retry = branch_state(&delete_db);
    let counts_before_delete_retry = mutation_guard_counts(&delete_db);
    let delete_retry = run(&[
        "delete-symbol",
        delete_db.to_str().unwrap(),
        "unused",
        "--expect-root",
        delete_expected_root,
    ]);
    assert!(delete_retry.contains("already_applied delete_symbol main.unused"));
    assert_eq!(branch_state(&delete_db), branch_before_delete_retry);
    assert_eq!(
        mutation_guard_counts(&delete_db),
        counts_before_delete_retry
    );

    let alias_db = temp.path().join("alias-retry.sqlite");
    run(&["init", alias_db.to_str().unwrap()]);
    run(&["import", alias_db.to_str().unwrap(), "examples/shop.cdb"]);
    let alias = run(&[
        "create-alias",
        alias_db.to_str().unwrap(),
        "tax",
        "sales_tax",
    ]);
    let alias_expected_root = parse_line_value(&alias, "old_root ");
    assert!(alias.contains("build_impact metadata_only"));
    let branch_before_alias_retry = branch_state(&alias_db);
    let counts_before_alias_retry = mutation_guard_counts(&alias_db);
    let alias_retry = run(&[
        "create-alias",
        alias_db.to_str().unwrap(),
        "tax",
        "sales_tax",
        "--expect-root",
        alias_expected_root,
    ]);
    assert!(alias_retry.contains("already_applied create_alias main.tax as main.sales_tax"));
    assert_eq!(branch_state(&alias_db), branch_before_alias_retry);
    assert_eq!(mutation_guard_counts(&alias_db), counts_before_alias_retry);
}

#[test]
fn stale_expected_root_conflicts_across_structural_operations() {
    let temp = tempdir().unwrap();

    let replace_db = temp.path().join("replace-conflict.sqlite");
    run(&["init", replace_db.to_str().unwrap()]);
    let replace_import = run(&["import", replace_db.to_str().unwrap(), "examples/shop.cdb"]);
    let replace_expected_root = parse_line_value(&replace_import, "root ");
    run(&[
        "create-alias",
        replace_db.to_str().unwrap(),
        "tax",
        "sales_tax",
    ]);
    let branch_before_replace_conflict = branch_state(&replace_db);
    let counts_before_replace_conflict = mutation_guard_counts(&replace_db);
    let replace_conflict = run(&[
        "replace-body",
        replace_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
        "--expect-root",
        replace_expected_root,
    ]);
    assert!(replace_conflict.contains("conflict replace_function_body main.tax"));
    assert_eq!(branch_state(&replace_db), branch_before_replace_conflict);
    assert_eq!(
        mutation_guard_counts(&replace_db),
        counts_before_replace_conflict
    );

    let signature_db = temp.path().join("signature-conflict.sqlite");
    let signature_source = temp.path().join("signature-conflict.cdb");
    std::fs::write(
        &signature_source,
        "fn ignore(x: i64) -> i64 = 1\n\nfn main() -> i64 = ignore(5)\n",
    )
    .unwrap();
    run(&["init", signature_db.to_str().unwrap()]);
    let signature_import = run(&[
        "import",
        signature_db.to_str().unwrap(),
        signature_source.to_str().unwrap(),
    ]);
    let signature_expected_root = parse_line_value(&signature_import, "root ");
    run(&[
        "create-alias",
        signature_db.to_str().unwrap(),
        "ignore",
        "ignored",
    ]);
    let branch_before_signature_conflict = branch_state(&signature_db);
    let counts_before_signature_conflict = mutation_guard_counts(&signature_db);
    let signature_conflict = run(&[
        "change-signature",
        signature_db.to_str().unwrap(),
        "ignore",
        "(y: i64) -> i64",
        "--expect-root",
        signature_expected_root,
    ]);
    assert!(signature_conflict.contains("conflict change_function_signature main.ignore"));
    assert_eq!(
        branch_state(&signature_db),
        branch_before_signature_conflict
    );
    assert_eq!(
        mutation_guard_counts(&signature_db),
        counts_before_signature_conflict
    );

    let delete_db = temp.path().join("delete-conflict.sqlite");
    let delete_source = temp.path().join("delete-conflict.cdb");
    std::fs::write(
        &delete_source,
        "fn unused() -> i64 = 1\n\nfn main() -> i64 = 2\n",
    )
    .unwrap();
    run(&["init", delete_db.to_str().unwrap()]);
    let delete_import = run(&[
        "import",
        delete_db.to_str().unwrap(),
        delete_source.to_str().unwrap(),
    ]);
    let delete_expected_root = parse_line_value(&delete_import, "root ");
    run(&[
        "create-alias",
        delete_db.to_str().unwrap(),
        "unused",
        "still_here",
    ]);
    let branch_before_delete_conflict = branch_state(&delete_db);
    let counts_before_delete_conflict = mutation_guard_counts(&delete_db);
    let delete_conflict = run(&[
        "delete-symbol",
        delete_db.to_str().unwrap(),
        "unused",
        "--expect-root",
        delete_expected_root,
    ]);
    assert!(delete_conflict.contains("conflict delete_symbol main.unused"));
    assert_eq!(branch_state(&delete_db), branch_before_delete_conflict);
    assert_eq!(
        mutation_guard_counts(&delete_db),
        counts_before_delete_conflict
    );

    let alias_db = temp.path().join("alias-conflict.sqlite");
    run(&["init", alias_db.to_str().unwrap()]);
    let alias_import = run(&["import", alias_db.to_str().unwrap(), "examples/shop.cdb"]);
    let alias_expected_root = parse_line_value(&alias_import, "root ");
    run(&[
        "replace-body",
        alias_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    let branch_before_alias_conflict = branch_state(&alias_db);
    let counts_before_alias_conflict = mutation_guard_counts(&alias_db);
    let alias_conflict = run(&[
        "create-alias",
        alias_db.to_str().unwrap(),
        "tax",
        "sales_tax",
        "--expect-root",
        alias_expected_root,
    ]);
    assert!(alias_conflict.contains("conflict create_alias main.tax as main.sales_tax"));
    assert_eq!(branch_state(&alias_db), branch_before_alias_conflict);
    assert_eq!(
        mutation_guard_counts(&alias_db),
        counts_before_alias_conflict
    );
}

#[test]
fn failed_applied_migration_rolls_back_partial_writes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("rollback.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let branch_before_failure = branch_state(&db);
    let counts_before_failure = mutation_guard_counts(&db);

    let stderr = run_failure(&["replace-body", db.to_str().unwrap(), "tax", "true"]);
    assert!(stderr.contains("replacement body type bool does not match return type i64"));
    assert_eq!(branch_state(&db), branch_before_failure);
    assert_eq!(mutation_guard_counts(&db), counts_before_failure);

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn verify_rejects_cache_key_payload_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("cache-mismatch.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, cache_key_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, cache_key_json FROM compile_cache
             WHERE artifact_kind = 'interface_hash'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&cache_key_json).unwrap();
    value["target_triple"] = JsonValue::String("aarch64-apple-darwin".to_string());
    let tampered = serde_json::to_string(&value).unwrap();
    conn.execute(
        "UPDATE compile_cache SET cache_key_json = ?1 WHERE cache_key = ?2",
        (&tampered, &cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_cache_entry"));
    assert!(stderr.contains("cache key mismatch"));
}

fn cache_rows(db: &Path) -> Vec<(String, String, String)> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT backend, target, artifact_kind FROM compile_cache ORDER BY artifact_kind")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn row_count(db: &Path, table: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn branch_state(db: &Path) -> (String, Option<String>) {
    let conn = Connection::open(db).unwrap();
    conn.query_row(
        "SELECT root_hash, history_hash FROM branches WHERE name = 'main'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn mutation_guard_counts(db: &Path) -> Vec<(String, i64)> {
    [
        "objects",
        "migrations",
        "histories",
        "root_symbols",
        "root_names",
        "dependencies",
        "compile_cache",
        "source_search",
    ]
    .into_iter()
    .map(|table| (table.to_string(), row_count(db, table)))
    .collect()
}

fn compile_and_run_c_if_available(c_file: &Path) {
    if StdCommand::new("cc").arg("--version").output().is_err() {
        return;
    }
    let dir = c_file.parent().unwrap();
    let harness = dir.join("harness.c");
    let exe = dir.join("harness");
    std::fs::write(
        &harness,
        "long codedb_main(void);\nint main(void) { return codedb_main() == 120 ? 0 : 1; }\n",
    )
    .unwrap();
    let status = StdCommand::new("cc")
        .arg(c_file)
        .arg(&harness)
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("run cc");
    assert!(status.success());
    let status = StdCommand::new(&exe).status().expect("run c harness");
    assert!(status.success());
}
