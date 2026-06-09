//! Proof-carrying receipts (PLAN_V3 Phase 3).
//!
//! Every structural write returns, before commit, a receipt bundling: the
//! typecheck verdict, the borrow-check invariant, the per-symbol effect delta and
//! the root capability-surface delta, the build-impact verdict, and the (hash-
//! pruned) semantic diff. This lets a concurrent agent bind a change to evidence
//! of its consequences without re-deriving them.

use std::path::Path;

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

fn workspace_call(db: &Path, method: &str, params: JsonValue) -> JsonValue {
    let mut codedb = codedb::CodeDb::open(db).unwrap();
    let response = codedb::workspace::execute_workspace_request(
        &mut codedb,
        codedb::workspace::WorkspaceRequest {
            schema: None,
            jsonrpc: Some("2.0".to_string()),
            method: method.to_string(),
            params,
            id: Some(json!(method)),
            request_id: None,
        },
    );
    serde_json::to_value(response).unwrap()
}

/// Assert a receipt object carries every required field with the right shape.
fn assert_complete_receipt(receipt: &JsonValue) {
    assert_eq!(receipt["schema"], "codedb/receipt/v1", "receipt: {receipt}");
    assert!(receipt["typecheck"].is_string(), "typecheck missing");
    assert!(receipt["build_impact"]["kind"].is_string(), "build_impact missing");
    assert!(receipt["borrow_check"]["checked"].is_boolean(), "borrow_check.checked");
    assert!(receipt["borrow_check"]["passed"].is_boolean(), "borrow_check.passed");
    assert!(
        receipt["borrow_check"]["affected_symbols"].is_array(),
        "borrow_check.affected_symbols"
    );
    assert!(receipt["effect_delta"]["changed"].is_array(), "effect_delta.changed");
    assert!(receipt["capability_delta"]["added"].is_array(), "capability_delta.added");
    assert!(receipt["capability_delta"]["removed"].is_array(), "capability_delta.removed");
    assert!(receipt["semantic_diff"].is_array(), "semantic_diff");
}

#[test]
fn structural_write_returns_a_complete_receipt() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("receipt.sqlite");
    let source = temp.path().join("receipt.cdb");
    std::fs::write(
        &source,
        "fn helper() -> i64 = 1\nfn f(x: i64) -> i64 = x + helper()\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let replaced = parse_json(&run(&["replace-body", path(&db), "f", "x + 2", "--json"]));
    assert_eq!(replaced["status"], "applied");
    let receipt = &replaced["summary"]["receipt"];
    assert_complete_receipt(receipt);

    // Applying ran type/borrow/effect checking on the new root, so the change is
    // proven to have passed for the affected symbols.
    assert_eq!(receipt["borrow_check"]["checked"], true);
    assert_eq!(receipt["borrow_check"]["passed"], true);
    assert_eq!(
        receipt["borrow_check"]["affected_symbols"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // The semantic diff embeds the hash-pruned body diff: only the changed
    // operand (`helper()` -> `2`) is reported.
    let semantic_diff = receipt["semantic_diff"].as_array().unwrap();
    let impl_change = semantic_diff
        .iter()
        .find(|change| change["kind"] == "implementation_changed")
        .expect("implementation_changed in semantic diff");
    let expr_changes = impl_change["expr_changes"].as_array().unwrap();
    assert!(
        expr_changes.iter().any(|change| change["path"] == "body/right"),
        "expr_changes: {expr_changes:?}"
    );
}

#[test]
fn receipt_tracks_effect_and_capability_deltas() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("receipt-effects.sqlite");
    let source = temp.path().join("receipt-effects.cdb");
    std::fs::write(&source, "fn f() -> i64 = 41\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    // Pure -> effects[io]: the per-symbol effect delta and the root capability
    // surface both gain `io`.
    let changed = parse_json(&run(&[
        "change-signature",
        path(&db),
        "f",
        "() -> i64 effects[io]",
        "--json",
    ]));
    assert_eq!(changed["status"], "applied");
    let receipt = &changed["summary"]["receipt"];
    assert_complete_receipt(receipt);

    let effect_changed = receipt["effect_delta"]["changed"].as_array().unwrap();
    assert_eq!(effect_changed.len(), 1, "effect_delta: {effect_changed:?}");
    assert_eq!(effect_changed[0]["name"], "f");
    assert!(
        effect_changed[0]["added"]
            .as_array()
            .unwrap()
            .contains(&json!("io")),
        "added effects: {:?}",
        effect_changed[0]["added"]
    );

    assert!(
        receipt["capability_delta"]["added"]
            .as_array()
            .unwrap()
            .contains(&json!("io")),
        "capability added: {:?}",
        receipt["capability_delta"]["added"]
    );
}

#[test]
fn preview_returns_a_complete_receipt_before_commit() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("receipt-preview.sqlite");
    let source = temp.path().join("receipt-preview.cdb");
    std::fs::write(&source, "fn f() -> i64 = 41\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let before = workspace_call(&db, "workspace.current", json!({}));
    let root = before["snapshot"]["root_hash"].as_str().unwrap().to_string();

    // ops.preview applies in a savepoint and rolls back: nothing is committed,
    // yet the full receipt is returned — proving it is available pre-commit.
    let preview = workspace_call(
        &db,
        "ops.preview",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": root,
            "operations": [
                { "kind": "rename_symbol", "name": "f", "new_name": "g" }
            ]
        }),
    );
    assert_eq!(preview["status"], "ok");
    assert_eq!(preview["result"]["committed"], false);
    assert_eq!(preview["result"]["preview"], true);
    let receipt = &preview["result"]["results"][0]["summary"]["receipt"];
    assert_complete_receipt(receipt);
    // A rename is metadata-only and changes no effects/capabilities.
    assert_eq!(receipt["build_impact"]["kind"], "metadata_only");

    // The branch did not move.
    let after = workspace_call(&db, "workspace.current", json!({}));
    assert_eq!(after["snapshot"]["root_hash"], root);
}
