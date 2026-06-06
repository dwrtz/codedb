use std::path::Path;

use assert_cmd::Command;
use codedb::CodeDb;
use codedb::workspace::{WorkspaceRequest, WorkspaceResponse, execute_workspace_request};
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
    parse_json(&std::fs::read_to_string(path).unwrap())
}

fn current_root(db: &Path) -> String {
    parse_json(&run(&["list", path(db), "--json"]))["root_hash"]
        .as_str()
        .expect("root hash")
        .to_string()
}

fn write_json(dir: &Path, name: &str, value: JsonValue) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    path
}

fn apply_branch(
    dir: &Path,
    db: &Path,
    name: &str,
    branch: &str,
    expected_root: &str,
    operation: JsonValue,
) -> JsonValue {
    let apply = write_json(
        dir,
        name,
        json!({
            "schema": "codedb/apply/v1",
            "branch": branch,
            "expect_root_hash": expected_root,
            "operations": [operation],
        }),
    );
    parse_json(&run(&["apply", path(db), "--json", path(&apply)]))
}

fn tax_body(rate: &str) -> JsonValue {
    json!({
        "kind": "binary",
        "op": "/",
        "left": {
            "kind": "binary",
            "op": "*",
            "left": {
                "kind": "param_name",
                "name": "subtotal",
            },
            "right": {
                "kind": "literal_i64",
                "value": rate,
            },
        },
        "right": {
            "kind": "literal_i64",
            "value": "100",
        },
    })
}

fn workspace_call(db: &mut CodeDb, method: &str, params: JsonValue) -> JsonValue {
    let response: WorkspaceResponse = execute_workspace_request(
        db,
        WorkspaceRequest {
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

#[test]
fn conservative_merge_applies_disjoint_symbol_changes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("disjoint.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);
    let base_root = current_root(&db);

    run(&[
        "branch",
        "create",
        path(&db),
        "agent/tax",
        "--from",
        "main",
        "--json",
    ]);
    apply_branch(
        temp.path(),
        &db,
        "tax-body.apply.json",
        "agent/tax",
        &base_root,
        json!({
            "kind": "replace_function_body",
            "name": "tax",
            "body": tax_body("18"),
        }),
    );

    let target_change = parse_json(&run(&[
        "replace-body",
        path(&db),
        "total",
        "subtotal + tax(subtotal) + 1",
        "--expect-root",
        &base_root,
        "--json",
    ]));
    assert_eq!(target_change["status"], "applied");
    let target_root = target_change["new_root_hash"].as_str().unwrap().to_string();

    let preview = parse_json(&run(&[
        "merge",
        "preview",
        path(&db),
        "main",
        "agent/tax",
        "--json",
    ]));
    assert_eq!(preview["schema"], "codedb/merge-result/v1");
    assert_eq!(preview["status"], "mergeable");
    assert_eq!(preview["target_unique_migration_count"], 1);
    assert_eq!(preview["source_unique_migration_count"], 1);

    let applied = parse_json(&run(&[
        "merge",
        "apply",
        path(&db),
        "main",
        "agent/tax",
        "--expect-root",
        &target_root,
        "--json",
    ]));
    assert_eq!(applied["status"], "merged");
    assert_eq!(applied["committed"], true);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "119");
    run(&["verify", path(&db)]);
}

#[test]
fn conservative_merge_combines_rename_and_body_change_and_replays_history() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("rename-body.sqlite");
    let imported = temp.path().join("imported.sqlite");
    let history = temp.path().join("history.ndjson");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);
    let base_root = current_root(&db);

    run(&[
        "branch",
        "create",
        path(&db),
        "agent/body",
        "--from",
        "main",
        "--json",
    ]);
    apply_branch(
        temp.path(),
        &db,
        "body.apply.json",
        "agent/body",
        &base_root,
        json!({
            "kind": "replace_function_body",
            "name": "tax",
            "body": tax_body("18"),
        }),
    );
    let rename = parse_json(&run(&[
        "rename",
        path(&db),
        "tax",
        "vat",
        "--expect-root",
        &base_root,
        "--json",
    ]));
    let target_root = rename["new_root_hash"].as_str().unwrap().to_string();

    let mut api_db = CodeDb::open(&db).unwrap();
    let api_preview = workspace_call(
        &mut api_db,
        "merge.preview",
        json!({"target": "main", "source": "agent/body"}),
    );
    assert_eq!(api_preview["status"], "ok");
    assert_eq!(api_preview["result"]["status"], "mergeable");

    let applied = parse_json(&run(&[
        "merge",
        "apply",
        path(&db),
        "main",
        "agent/body",
        "--expect-root",
        &target_root,
        "--json",
    ]));
    assert_eq!(applied["status"], "merged");
    assert_eq!(
        applied["operation_result"]["summary"]["kind"],
        "merge_branch"
    );
    let show_vat = parse_json(&run(&["show", path(&db), "vat", "--json"]));
    assert_eq!(show_vat["body_source"], "subtotal * 18 / 100");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "118");
    run(&["verify", path(&db)]);

    run(&[
        "export-history",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&history),
    ]);
    run(&["init", path(&imported)]);
    run(&["import-history", path(&imported), path(&history)]);
    run(&["verify", path(&imported)]);
    assert_eq!(run(&["eval", path(&imported), "main"]).trim(), "118");
    let imported_vat = parse_json(&run(&["show", path(&imported), "vat", "--json"]));
    assert_eq!(imported_vat["body_source"], "subtotal * 18 / 100");
}

#[test]
fn conservative_merge_reports_semantic_conflicts() {
    let temp = tempdir().unwrap();

    let body_db = temp.path().join("body-conflict.sqlite");
    run(&["init", path(&body_db)]);
    run(&["import", path(&body_db), "examples/shop.cdb"]);
    let body_base = current_root(&body_db);
    run(&[
        "branch",
        "create",
        path(&body_db),
        "agent/body",
        "--from",
        "main",
        "--json",
    ]);
    apply_branch(
        temp.path(),
        &body_db,
        "body-conflict.apply.json",
        "agent/body",
        &body_base,
        json!({
            "kind": "replace_function_body",
            "name": "tax",
            "body": tax_body("18"),
        }),
    );
    run(&[
        "replace-body",
        path(&body_db),
        "tax",
        "subtotal * 25 / 100",
        "--expect-root",
        &body_base,
        "--json",
    ]);
    let body_conflict = parse_json(&run(&[
        "merge",
        "preview",
        path(&body_db),
        "main",
        "agent/body",
        "--json",
    ]));
    assert_eq!(body_conflict["status"], "conflict");
    assert_eq!(body_conflict["conflicts"][0]["kind"], "dependency_conflict");

    let name_db = temp.path().join("name-conflict.sqlite");
    run(&["init", path(&name_db)]);
    run(&["import", path(&name_db), "examples/shop.cdb"]);
    let name_base = current_root(&name_db);
    run(&[
        "branch",
        "create",
        path(&name_db),
        "agent/name",
        "--from",
        "main",
        "--json",
    ]);
    apply_branch(
        temp.path(),
        &name_db,
        "name-conflict.apply.json",
        "agent/name",
        &name_base,
        json!({
            "kind": "rename_symbol",
            "name": "total",
            "new_name": "amount",
        }),
    );
    run(&[
        "rename",
        path(&name_db),
        "tax",
        "amount",
        "--expect-root",
        &name_base,
        "--json",
    ]);
    let name_conflict = parse_json(&run(&[
        "merge",
        "preview",
        path(&name_db),
        "main",
        "agent/name",
        "--json",
    ]));
    assert_eq!(name_conflict["status"], "conflict");
    assert_eq!(name_conflict["conflicts"][0]["kind"], "name_conflict");

    let export_db = temp.path().join("export-conflict.sqlite");
    run(&["init", path(&export_db)]);
    run(&["import", path(&export_db), "examples/shop.cdb"]);
    let export_base = current_root(&export_db);
    run(&[
        "branch",
        "create",
        path(&export_db),
        "agent/export",
        "--from",
        "main",
        "--json",
    ]);
    apply_branch(
        temp.path(),
        &export_db,
        "export-conflict.apply.json",
        "agent/export",
        &export_base,
        json!({
            "kind": "set_export",
            "name": "total",
            "exported_name": "shared",
        }),
    );
    run(&[
        "set-export",
        path(&export_db),
        "tax",
        "shared",
        "--expect-root",
        &export_base,
        "--json",
    ]);
    let export_conflict = parse_json(&run(&[
        "merge",
        "preview",
        path(&export_db),
        "main",
        "agent/export",
        "--json",
    ]));
    assert_eq!(export_conflict["status"], "conflict");
    assert_eq!(export_conflict["conflicts"][0]["kind"], "export_conflict");

    let delete_use_db = temp.path().join("delete-use-conflict.sqlite");
    let delete_use_source = temp.path().join("delete-use-conflict.cdb");
    std::fs::write(
        &delete_use_source,
        "fn helper() -> i64 = 7\n\
         \n\
         fn main() -> i64 = 1\n",
    )
    .unwrap();
    run(&["init", path(&delete_use_db)]);
    run(&["import", path(&delete_use_db), path(&delete_use_source)]);
    let delete_use_base = current_root(&delete_use_db);
    run(&[
        "branch",
        "create",
        path(&delete_use_db),
        "agent/delete",
        "--from",
        "main",
        "--json",
    ]);
    apply_branch(
        temp.path(),
        &delete_use_db,
        "delete-use-conflict.apply.json",
        "agent/delete",
        &delete_use_base,
        json!({
            "kind": "delete_symbol",
            "name": "helper",
            "force": true,
        }),
    );
    run(&[
        "replace-body",
        path(&delete_use_db),
        "main",
        "helper()",
        "--expect-root",
        &delete_use_base,
        "--json",
    ]);
    let delete_use_conflict = parse_json(&run(&[
        "merge",
        "preview",
        path(&delete_use_db),
        "main",
        "agent/delete",
        "--json",
    ]));
    assert_eq!(delete_use_conflict["status"], "conflict");
    assert_eq!(
        delete_use_conflict["conflicts"][0]["kind"],
        "dependency_conflict"
    );
    assert!(
        !delete_use_conflict["conflicts"][0]["details"]["error"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn conservative_merge_preserves_type_added_on_source() {
    // Regression: branch merge must reconcile type definitions, not silently
    // keep the target's. A record added on the source branch (disjoint from a
    // target-only function change) must survive the merge into main.
    let temp = tempdir().unwrap();
    let db = temp.path().join("merge-add-type.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);
    let base_root = current_root(&db);

    run(&[
        "branch",
        "create",
        path(&db),
        "agent/types",
        "--from",
        "main",
        "--json",
    ]);
    let added = apply_branch(
        temp.path(),
        &db,
        "add-money.apply.json",
        "agent/types",
        &base_root,
        json!({
            "kind": "create_type",
            "name": "Money",
            "definition": {
                "kind": "record",
                "fields": [{ "name": "cents", "type": "i64" }],
            },
        }),
    );
    assert_eq!(added["status"], "applied");

    let target_change = parse_json(&run(&[
        "replace-body",
        path(&db),
        "total",
        "subtotal + 1",
        "--expect-root",
        &base_root,
        "--json",
    ]));
    assert_eq!(target_change["status"], "applied");
    let target_root = target_change["new_root_hash"].as_str().unwrap().to_string();

    let applied = parse_json(&run(&[
        "merge",
        "apply",
        path(&db),
        "main",
        "agent/types",
        "--expect-root",
        &target_root,
        "--json",
    ]));
    assert_eq!(applied["status"], "merged");
    assert_eq!(applied["committed"], true);

    // The added record must resolve in the merged main root: emit its layout.
    let layout_path = temp.path().join("money.layout.json");
    run(&[
        "emit-type-layout",
        path(&db),
        "Money",
        "--out",
        path(&layout_path),
    ]);
    let layout = read_json(&layout_path);
    assert_eq!(layout["kind"], "record");
    assert_eq!(layout["fields"][0]["name"], "cents");
    // The disjoint target-side change must also be present (total = subtotal + 1,
    // main = total(100)).
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "101");
    run(&["verify", path(&db)]);
}

#[test]
fn conservative_merge_reports_type_definition_conflict() {
    // Regression: when both branches change the same type definition divergently
    // (here renaming the same field two different ways), the merge must report a
    // conflict instead of silently keeping the target's definition.
    let temp = tempdir().unwrap();
    let db = temp.path().join("merge-type-conflict.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);
    let root0 = current_root(&db);

    // Put Money into the common ancestor on main.
    let base = apply_branch(
        temp.path(),
        &db,
        "money-base.apply.json",
        "main",
        &root0,
        json!({
            "kind": "create_type",
            "name": "Money",
            "definition": {
                "kind": "record",
                "fields": [{ "name": "cents", "type": "i64" }],
            },
        }),
    );
    let base_root = base["new_root_hash"].as_str().unwrap().to_string();

    run(&[
        "branch",
        "create",
        path(&db),
        "agent/field",
        "--from",
        "main",
        "--json",
    ]);
    apply_branch(
        temp.path(),
        &db,
        "rename-pennies.apply.json",
        "agent/field",
        &base_root,
        json!({
            "kind": "rename_field",
            "type": "Money",
            "field": "cents",
            "new_name": "pennies",
        }),
    );
    apply_branch(
        temp.path(),
        &db,
        "rename-units.apply.json",
        "main",
        &base_root,
        json!({
            "kind": "rename_field",
            "type": "Money",
            "field": "cents",
            "new_name": "units",
        }),
    );

    let conflict = parse_json(&run(&[
        "merge",
        "preview",
        path(&db),
        "main",
        "agent/field",
        "--json",
    ]));
    assert_eq!(conflict["status"], "conflict");
    assert_eq!(conflict["conflicts"][0]["kind"], "dependency_conflict");
}
