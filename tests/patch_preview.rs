use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn path(path: &std::path::Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn current_root(db: &std::path::Path) -> String {
    parse_json(&run(&["list", path(db), "--json"]))["root_hash"]
        .as_str()
        .expect("root hash")
        .to_string()
}

fn branch_state(db: &std::path::Path) -> (String, Option<String>) {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT root_hash, history_hash FROM branches WHERE name = 'main'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn mutation_counts(db: &std::path::Path) -> Vec<(String, i64)> {
    [
        "objects",
        "object_edges",
        "migrations",
        "histories",
        "branches",
        "root_symbols",
        "root_names",
        "root_exports",
        "dependencies",
        "compile_cache",
        "artifact_jobs",
        "workspace_transactions",
        "source_search",
    ]
    .into_iter()
    .map(|table| (table.to_string(), row_count(db, table)))
    .collect()
}

fn row_count(db: &std::path::Path, table: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn write_patch(dir: &std::path::Path, name: &str, patch: JsonValue) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(&patch).unwrap()).unwrap();
    path
}

#[test]
fn semantic_patch_preview_replaces_literal_without_committing() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("literal-preview.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root = current_root(&db);
    let branch_before = branch_state(&db);
    let counts_before = mutation_counts(&db);
    let patch = write_patch(
        temp.path(),
        "literal.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "literal_i64",
                "value": "20",
                "within_name": "tax"
            },
            "replace": {
                "kind": "literal_i64",
                "value": "18"
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
    assert_eq!(preview["schema"], "codedb/semantic-patch-preview/v1");
    assert_eq!(preview["status"], "planned");
    assert_eq!(preview["root_hash"], root);
    assert_eq!(preview["match_count"], 1);
    assert_eq!(
        preview["matched_expressions"][0]["expr_kind"],
        "literal_i64"
    );
    assert_eq!(preview["matched_expressions"][0]["symbol_name"], "tax");
    assert_eq!(
        preview["matched_expressions"][0]["literal_value"],
        json!({"kind": "i64", "value": "20"})
    );
    assert_eq!(preview["planned_operation_count"], 1);
    assert_eq!(
        preview["planned_operations"][0]["kind"],
        "replace_function_body"
    );
    assert_eq!(preview["planned_operations"][0]["name"], "tax");
    assert_eq!(preview["typecheck"]["status"], "ok");
    assert_eq!(preview["build_impact"]["kind"], "recompile_symbols");
    assert_eq!(preview["apply_preview"]["preview"], true);
    assert_eq!(preview["apply_preview"]["would_commit"], true);
    assert_eq!(preview["apply_preview"]["committed"], false);
    assert_ne!(preview["apply_preview"]["new_root_hash"], root);

    assert_eq!(branch_state(&db), branch_before);
    assert_eq!(mutation_counts(&db), counts_before);
    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    assert_eq!(show_tax["body_source"], "subtotal * 20 / 100");
}

#[test]
fn semantic_patch_preview_finds_and_retargets_call_expressions() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("shop-fee.cdb");
    std::fs::write(
        &source,
        "fn tax(subtotal: i64) -> i64 = subtotal * 20 / 100\n\
         \n\
         fn fee(subtotal: i64) -> i64 = subtotal * 5 / 100\n\
         \n\
         fn total(subtotal: i64) -> i64 = subtotal + tax(subtotal)\n\
         \n\
         fn main() -> i64 = total(100)\n",
    )
    .unwrap();
    let db = temp.path().join("call-preview.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    let root = current_root(&db);
    let branch_before = branch_state(&db);
    let counts_before = mutation_counts(&db);
    let patch = write_patch(
        temp.path(),
        "call.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "call",
                "target_name": "tax",
                "within_name": "total"
            },
            "replace": {
                "kind": "call",
                "target_name": "fee",
                "args": "$same_args"
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
    assert_eq!(preview["match_count"], 1);
    assert_eq!(preview["matched_expressions"][0]["expr_kind"], "call");
    assert_eq!(preview["matched_expressions"][0]["symbol_name"], "total");
    assert_eq!(preview["matched_expressions"][0]["target_name"], "tax");
    assert_eq!(
        preview["planned_operations"][0]["kind"],
        "replace_function_body"
    );
    assert_eq!(preview["planned_operations"][0]["name"], "total");
    assert_eq!(preview["typecheck"]["status"], "ok");
    assert_eq!(preview["build_impact"]["kind"], "recompile_symbols");

    assert_eq!(branch_state(&db), branch_before);
    assert_eq!(mutation_counts(&db), counts_before);
    let show_total = parse_json(&run(&["show", path(&db), "total", "--json"]));
    assert_eq!(show_total["body_source"], "subtotal + tax(subtotal)");
}

#[test]
fn semantic_patch_preview_covers_initial_operation_surface() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("patch-surface.cdb");
    std::fs::write(
        &source,
        "fn tax(subtotal: i64) -> i64 = subtotal * 20 / 100\n\
         \n\
         fn unused() -> i64 = 7\n\
         \n\
         fn total(subtotal: i64) -> i64 = subtotal + tax(subtotal)\n\
         \n\
         fn main() -> i64 = total(100)\n",
    )
    .unwrap();
    let db = temp.path().join("patch-surface-preview.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let root = current_root(&db);
    run(&[
        "set-export",
        path(&db),
        "unused",
        "unused_api",
        "--expect-root",
        &root,
        "--json",
    ]);
    let root = current_root(&db);
    let branch_before = branch_state(&db);
    let counts_before = mutation_counts(&db);
    let cases = [
        (
            "extract.patch.json",
            json!({
                "schema": "codedb/semantic-patch/v1",
                "branch": "main",
                "expected_root": root,
                "match": {
                    "kind": "literal_i64",
                    "value": "20",
                    "within_name": "tax"
                },
                "replace": {
                    "kind": "extract_function",
                    "name": "rate"
                }
            }),
            json!(["create_function", "replace_function_body"]),
        ),
        (
            "inline.patch.json",
            json!({
                "schema": "codedb/semantic-patch/v1",
                "branch": "main",
                "expected_root": root,
                "match": {
                    "kind": "call",
                    "target_name": "tax",
                    "within_name": "total"
                },
                "replace": {
                    "kind": "inline_function"
                }
            }),
            json!(["replace_function_body"]),
        ),
        (
            "add-param.patch.json",
            json!({
                "schema": "codedb/semantic-patch/v1",
                "branch": "main",
                "expected_root": root,
                "match": {
                    "kind": "symbol",
                    "name": "unused"
                },
                "replace": {
                    "kind": "add_parameter",
                    "name": "scale",
                    "type": "i64",
                    "default": {
                        "kind": "literal_i64",
                        "value": "1"
                    }
                }
            }),
            json!(["add_parameter"]),
        ),
        (
            "set-export.patch.json",
            json!({
                "schema": "codedb/semantic-patch/v1",
                "branch": "main",
                "expected_root": root,
                "match": {
                    "kind": "symbol",
                    "name": "unused"
                },
                "replace": {
                    "kind": "set_export",
                    "exported_name": "unused_api_extra"
                }
            }),
            json!(["set_export"]),
        ),
        (
            "remove-export.patch.json",
            json!({
                "schema": "codedb/semantic-patch/v1",
                "branch": "main",
                "expected_root": root,
                "match": {
                    "kind": "export",
                    "exported_name": "unused_api"
                },
                "replace": {
                    "kind": "remove_export",
                    "exported_name": "unused_api"
                }
            }),
            json!(["remove_export"]),
        ),
        (
            "remove-unused.patch.json",
            json!({
                "schema": "codedb/semantic-patch/v1",
                "branch": "main",
                "expected_root": root,
                "match": {
                    "kind": "symbol",
                    "name": "unused"
                },
                "replace": {
                    "kind": "remove_unused_symbol"
                }
            }),
            json!(["delete_symbol"]),
        ),
    ];

    for (file_name, patch, expected_kinds) in cases {
        let patch = write_patch(temp.path(), file_name, patch);
        let preview = parse_json(&run(&[
            "patch",
            "preview",
            path(&db),
            "--json",
            path(&patch),
        ]));
        assert_eq!(preview["status"], "planned", "{file_name}");
        assert_eq!(preview["typecheck"]["status"], "ok", "{file_name}");
        let operation_kinds = preview["planned_operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|operation| operation["kind"].clone())
            .collect::<Vec<_>>();
        assert_eq!(json!(operation_kinds), expected_kinds, "{file_name}");
    }

    assert_eq!(branch_state(&db), branch_before);
    assert_eq!(mutation_counts(&db), counts_before);
}

#[test]
fn semantic_patch_preview_reports_type_errors_before_apply() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("type-error-preview.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);
    let root = current_root(&db);
    let branch_before = branch_state(&db);
    let counts_before = mutation_counts(&db);
    let patch = write_patch(
        temp.path(),
        "type-error.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "literal_i64",
                "value": "20",
                "within_name": "tax"
            },
            "replace": {
                "kind": "literal_bool",
                "value": true
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
    assert_eq!(preview["status"], "error");
    assert_eq!(preview["typecheck"]["status"], "error");
    assert!(
        preview["typecheck"]["message"]
            .as_str()
            .unwrap()
            .contains("expected a sized integer right operand, got bool")
    );
    assert_eq!(preview["diagnostics"][0]["kind"], "type_error");
    assert_eq!(preview["apply_preview"]["status"], "error");
    assert_eq!(preview["apply_preview"]["committed"], false);
    assert_eq!(preview["apply_preview"]["rollback_reason"], "error");

    assert_eq!(branch_state(&db), branch_before);
    assert_eq!(mutation_counts(&db), counts_before);
    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    assert_eq!(show_tax["body_source"], "subtotal * 20 / 100");
}

#[test]
fn workspace_patch_preview_returns_snapshot_without_mutating() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-patch-preview.sqlite");
    let mut db = codedb::CodeDb::open(&db_path).unwrap();
    db.init().unwrap();
    db.import_file(std::path::Path::new("examples/shop.cdb"))
        .unwrap();
    let before = branch_state(&db_path);
    let counts_before = mutation_counts(&db_path);

    let response = codedb::workspace::execute_workspace_request(
        &mut db,
        codedb::workspace::WorkspaceRequest {
            schema: None,
            jsonrpc: Some("2.0".to_string()),
            method: "patch.preview".to_string(),
            params: json!({
                "schema": "codedb/semantic-patch/v1",
                "branch": "main",
                "expected_root": before.0,
                "match": {
                    "kind": "literal_i64",
                    "value": "20",
                    "within_name": "tax"
                },
                "replace": {
                    "kind": "literal_i64",
                    "value": "18"
                }
            }),
            id: Some(json!("patch.preview")),
            request_id: None,
        },
    );
    let response = serde_json::to_value(response).unwrap();
    assert_eq!(response["schema"], "codedb/response/v1");
    assert_eq!(response["status"], "ok");
    assert_eq!(response["snapshot"]["root_hash"], before.0);
    assert_eq!(response["snapshot"]["history_hash"], json!(before.1));
    assert_eq!(
        response["result"]["schema"],
        "codedb/semantic-patch-preview/v1"
    );
    assert_eq!(response["result"]["status"], "planned");
    assert_eq!(response["result"]["planned_operation_count"], 1);

    assert_eq!(branch_state(&db_path), before);
    assert_eq!(mutation_counts(&db_path), counts_before);
}
