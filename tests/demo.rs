use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
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

    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root]);
    assert!(diff.contains("symbol_renamed"));
    assert!(diff.contains("main.tax -> main.vat"));
    assert!(diff.contains("function body hash: unchanged"));

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

    bin()
        .args(["eval", db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("118\n");

    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root]);
    assert!(diff.contains("implementation_changed"));
    assert!(diff.contains("signature: unchanged"));
    assert!(diff.contains("literal_changed: 20 -> 18"));

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success();
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
