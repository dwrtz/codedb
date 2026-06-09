//! Hash-pruned tree diff (PLAN_V3 Phase 3).
//!
//! The semantic diff descends into a changed function body only where content
//! hashes differ. Because every expression node's hash is a Merkle hash over its
//! transitive children, an equal hash means an identical subtree, which is
//! skipped entirely. These tests pin both halves of the contract: a real nested
//! change is always found, and unchanged siblings never appear.

use std::path::Path;

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

/// Import `source`, replace `function`'s body with `new_body`, and return
/// `(root_before, root_after)`.
fn import_then_replace(
    db: &Path,
    source_path: &Path,
    source: &str,
    function: &str,
    new_body: &str,
) -> (String, String) {
    std::fs::write(source_path, source).unwrap();
    run(&["init", path(db)]);
    run(&["import", path(db), path(source_path)]);
    let replaced = parse_json(&run(&[
        "replace-body",
        path(db),
        function,
        new_body,
        "--json",
    ]));
    (
        replaced["old_root_hash"].as_str().unwrap().to_string(),
        replaced["new_root_hash"].as_str().unwrap().to_string(),
    )
}

fn implementation_change(diff: &JsonValue) -> &JsonValue {
    diff["changes"]
        .as_array()
        .expect("changes array")
        .iter()
        .find(|change| change["kind"] == "implementation_changed")
        .expect("an implementation_changed record")
}

#[test]
fn nested_change_reports_only_the_changed_leaf_and_prunes_siblings() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("diff-nested.sqlite");
    let source = temp.path().join("diff-nested.cdb");

    // Only the else branch's right operand literal changes (2 -> 3). The cond
    // (`x > 0`), the then branch (`x + 1`), and the else branch's left operand
    // (`x`) are all unchanged and must be pruned.
    let (root_a, root_b) = import_then_replace(
        &db,
        &source,
        "fn f(x: i64) -> i64 = if x > 0 then x + 1 else x + 2\n",
        "f",
        "if x > 0 then x + 1 else x + 3",
    );

    let diff = parse_json(&run(&["diff", path(&db), &root_a, &root_b, "--json"]));
    let change = implementation_change(&diff);
    let expr_changes = change["expr_changes"].as_array().expect("expr_changes");

    // Exactly the one changed leaf is reported, located precisely.
    assert_eq!(expr_changes.len(), 1, "expr_changes: {expr_changes:?}");
    assert_eq!(expr_changes[0]["kind"], "literal_changed");
    assert_eq!(expr_changes[0]["path"], "body/else/right");
    assert_eq!(expr_changes[0]["from"], "2");
    assert_eq!(expr_changes[0]["to"], "3");

    // Pruning: nothing from the unchanged cond/then subtrees appears.
    for change in expr_changes {
        let path = change["path"].as_str().unwrap_or("");
        assert!(
            !path.starts_with("body/cond") && !path.starts_with("body/then"),
            "unchanged subtree leaked into the diff: {path}"
        );
    }
}

#[test]
fn equal_roots_produce_no_changes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("diff-equal.sqlite");
    let source = temp.path().join("diff-equal.cdb");

    // Use replace-body only to capture a concrete root hash; then diff it
    // against itself.
    let (_, root) = import_then_replace(
        &db,
        &source,
        "fn f(x: i64) -> i64 = x + 1\n",
        "f",
        "x + 2",
    );

    let diff = parse_json(&run(&["diff", path(&db), &root, &root, "--json"]));
    assert_eq!(
        diff["changes"].as_array().expect("changes array").len(),
        0,
        "a root diffed against itself must report no changes"
    );
}

#[test]
fn changed_operator_reports_expression_replaced_at_the_node() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("diff-op.sqlite");
    let source = temp.path().join("diff-op.cdb");

    // The operator changes (+ -> -) but both operands are identical and pruned.
    let (root_a, root_b) = import_then_replace(
        &db,
        &source,
        "fn f(x: i64) -> i64 = x + 1\n",
        "f",
        "x - 1",
    );

    let diff = parse_json(&run(&["diff", path(&db), &root_a, &root_b, "--json"]));
    let change = implementation_change(&diff);
    let expr_changes = change["expr_changes"].as_array().expect("expr_changes");

    assert_eq!(expr_changes.len(), 1, "expr_changes: {expr_changes:?}");
    assert_eq!(expr_changes[0]["kind"], "expression_replaced");
    assert_eq!(expr_changes[0]["path"], "body");
    assert_eq!(expr_changes[0]["from"], "binary +");
    assert_eq!(expr_changes[0]["to"], "binary -");
}
