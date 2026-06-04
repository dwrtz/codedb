use std::path::Path;
use std::sync::{Arc, Barrier};

use assert_cmd::Command;
use codedb::CodeDb;
use codedb::workspace::{WorkspaceRequest, WorkspaceResponse, execute_workspace_request};
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

fn row_count(db: &Path, table: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn mutation_counts(db: &Path) -> Vec<(String, i64)> {
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

fn write_patch(dir: &Path, name: &str, patch: JsonValue) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(&patch).unwrap()).unwrap();
    path
}

fn enter_event_for_symbol<'a>(trace: &'a JsonValue, symbol_hash: &str) -> &'a JsonValue {
    trace["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|event| {
            event["event"] == "enter_function" && event["symbol_hash"].as_str() == Some(symbol_hash)
        })
        .unwrap_or_else(|| panic!("missing enter_function event for {symbol_hash}"))
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
fn semantic_patch_apply_replaces_literal_and_records_provenance() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("literal-apply.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root = current_root(&db);
    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    let tax_symbol = show_tax["symbol_hash"].as_str().unwrap();
    let before_trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let before_tax_enter = enter_event_for_symbol(&before_trace, tax_symbol);
    assert_eq!(
        before_trace["result"],
        json!({"kind": "i64", "value": "120"})
    );
    let patch = write_patch(
        temp.path(),
        "literal.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "agent": {
                "agent_id": "agent:patch-test",
                "request_id": "tax-rate-18"
            },
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

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["schema"], "codedb/semantic-patch-apply-result/v1");
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["committed"], true);
    assert_eq!(applied["old_root_hash"], root);
    assert_ne!(applied["new_root_hash"], root);
    assert_eq!(applied["match_count"], 1);
    assert_eq!(applied["planned_operation_count"], 1);
    assert_eq!(
        applied["planned_operations"][0]["kind"],
        "replace_function_body"
    );
    assert_eq!(applied["semantic_summary"]["matched_expression_count"], 1);
    assert_eq!(
        applied["semantic_summary"]["operation_kinds"],
        json!(["replace_function_body"])
    );
    assert_eq!(applied["build_impact"]["kind"], "recompile_symbols");
    assert_eq!(applied["apply_result"]["committed"], true);

    let show_tax_after = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    assert_eq!(show_tax_after["body_source"], "subtotal * 18 / 100");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "118");
    let after_trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let after_tax_enter = enter_event_for_symbol(&after_trace, tax_symbol);
    assert_eq!(
        after_trace["result"],
        json!({"kind": "i64", "value": "118"})
    );
    assert_ne!(
        before_tax_enter["function_def_hash"],
        after_tax_enter["function_def_hash"]
    );
    assert!(
        after_trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| {
                event["event"] == "value" && event["value"] == json!({"kind": "i64", "value": "18"})
            })
    );

    let agent_json: String = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT agent_json FROM migrations
             WHERE operation_kind = 'replace_function_body'
             ORDER BY created_at DESC, hash DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let agent: JsonValue = serde_json::from_str(&agent_json).unwrap();
    assert_eq!(agent["agent_id"], "agent:patch-test");
    assert_eq!(agent["request_id"], "tax-rate-18");
    assert_eq!(
        agent["semantic_patch"]["schema"],
        "codedb/semantic-patch-provenance/v1"
    );
    assert_eq!(agent["semantic_patch"]["patch_hash"], applied["patch_hash"]);
    assert_eq!(
        agent["semantic_patch"]["planned_operation_kinds"],
        json!(["replace_function_body"])
    );
}

#[test]
fn semantic_patch_apply_retries_expected_root_without_duplicate_history() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("literal-retry.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root = current_root(&db);
    let patch = write_patch(
        temp.path(),
        "retry.patch.json",
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

    let first = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(first["status"], "applied");
    let migration_count = row_count(&db, "migrations");
    let history_count = row_count(&db, "histories");

    let retry = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(retry["status"], "already_applied");
    assert_eq!(retry["committed"], false);
    assert_eq!(retry["new_root_hash"], first["new_root_hash"]);
    assert_eq!(row_count(&db, "migrations"), migration_count);
    assert_eq!(row_count(&db, "histories"), history_count);
}

#[test]
fn semantic_patch_apply_retries_multi_operation_patch_without_duplicate_history() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("extract-retry.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root = current_root(&db);
    let patch = write_patch(
        temp.path(),
        "extract-retry.patch.json",
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
    );

    let first = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(first["status"], "applied");
    assert_eq!(first["planned_operation_count"], 2);
    let migration_count = row_count(&db, "migrations");
    let history_count = row_count(&db, "histories");

    let retry = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(retry["status"], "already_applied");
    assert_eq!(retry["committed"], false);
    assert_eq!(retry["new_root_hash"], first["new_root_hash"]);
    assert_eq!(retry["apply_result"]["processed_operation_count"], 2);
    assert_eq!(row_count(&db, "migrations"), migration_count);
    assert_eq!(row_count(&db, "histories"), history_count);
}

#[test]
fn concurrent_duplicate_multi_operation_patch_apply_is_retry_safe() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("extract-concurrent-retry.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let root = current_root(&db);
    let migration_count = row_count(&db, "migrations");
    let history_count = row_count(&db, "histories");
    let patch_text = Arc::new(
        serde_json::to_string(&json!({
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
        }))
        .unwrap(),
    );
    let barrier = Arc::new(Barrier::new(4));
    let handles = (0..4)
        .map(|_| {
            let db = db.clone();
            let patch_text = Arc::clone(&patch_text);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut codedb = CodeDb::open(&db).unwrap();
                barrier.wait();
                let result = codedb.apply_semantic_patch_json_str(&patch_text).unwrap();
                serde_json::from_str::<JsonValue>(result.trim_end()).unwrap()
            })
        })
        .collect::<Vec<_>>();

    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let statuses = results
        .iter()
        .map(|result| result["status"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == "applied")
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == "already_applied")
            .count(),
        3
    );
    assert!(
        results
            .iter()
            .all(|result| result["committed"].as_bool() == Some(result["status"] == "applied"))
    );
    assert_eq!(row_count(&db, "migrations"), migration_count + 2);
    assert_eq!(row_count(&db, "histories"), history_count + 2);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");
}

#[test]
fn semantic_patch_apply_covers_initial_operation_surface() {
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
    let db = temp.path().join("patch-surface-apply.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let mut root = current_root(&db);
    let extract_patch = write_patch(
        temp.path(),
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
    );
    let extracted = parse_json(&run(&[
        "patch",
        "apply",
        path(&db),
        "--json",
        path(&extract_patch),
    ]));
    assert_eq!(extracted["status"], "applied");
    assert_eq!(
        extracted["semantic_summary"]["operation_kinds"],
        json!(["create_function", "replace_function_body"])
    );
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");
    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    assert_eq!(show_tax["body_source"], "subtotal * rate() / 100");
    root = extracted["new_root_hash"].as_str().unwrap().to_string();

    let inline_patch = write_patch(
        temp.path(),
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
    );
    let inlined = parse_json(&run(&[
        "patch",
        "apply",
        path(&db),
        "--json",
        path(&inline_patch),
    ]));
    assert_eq!(inlined["status"], "applied");
    assert_eq!(
        inlined["semantic_summary"]["operation_kinds"],
        json!(["replace_function_body"])
    );
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");
    root = inlined["new_root_hash"].as_str().unwrap().to_string();

    let add_param_patch = write_patch(
        temp.path(),
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
    );
    let added_param = parse_json(&run(&[
        "patch",
        "apply",
        path(&db),
        "--json",
        path(&add_param_patch),
    ]));
    assert_eq!(added_param["status"], "applied");
    assert_eq!(
        added_param["semantic_summary"]["operation_kinds"],
        json!(["add_parameter"])
    );
    let show_unused = parse_json(&run(&["show", path(&db), "unused", "--json"]));
    assert_eq!(show_unused["signature"], "(scale: i64) -> i64");
    root = added_param["new_root_hash"].as_str().unwrap().to_string();

    let set_export_patch = write_patch(
        temp.path(),
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
                "exported_name": "unused_api"
            }
        }),
    );
    let exported = parse_json(&run(&[
        "patch",
        "apply",
        path(&db),
        "--json",
        path(&set_export_patch),
    ]));
    assert_eq!(exported["status"], "applied");
    assert_eq!(
        exported["semantic_summary"]["operation_kinds"],
        json!(["set_export"])
    );
    root = exported["new_root_hash"].as_str().unwrap().to_string();

    let remove_export_patch = write_patch(
        temp.path(),
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
    );
    let unexported = parse_json(&run(&[
        "patch",
        "apply",
        path(&db),
        "--json",
        path(&remove_export_patch),
    ]));
    assert_eq!(unexported["status"], "applied");
    assert_eq!(
        unexported["semantic_summary"]["operation_kinds"],
        json!(["remove_export"])
    );
    root = unexported["new_root_hash"].as_str().unwrap().to_string();

    let remove_patch = write_patch(
        temp.path(),
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
    );
    let removed = parse_json(&run(&[
        "patch",
        "apply",
        path(&db),
        "--json",
        path(&remove_patch),
    ]));
    assert_eq!(removed["status"], "applied");
    assert_eq!(
        removed["semantic_summary"]["operation_kinds"],
        json!(["delete_symbol"])
    );
    let listed = parse_json(&run(&["list", path(&db), "--json"]));
    assert!(
        listed["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|symbol| symbol["name"] != "unused")
    );
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "120");
}

#[test]
fn semantic_patch_add_parameter_default_updates_callers_atomically() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("add-param-callers.cdb");
    std::fs::write(
        &source,
        "fn inc(x: i64) -> i64 = x + 1\n\
         \n\
         fn main() -> i64 = inc(2)\n",
    )
    .unwrap();
    let db = temp.path().join("add-param-callers.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let root = current_root(&db);
    let patch = write_patch(
        temp.path(),
        "add-param-callers.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "symbol",
                "name": "inc"
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
    );

    let preview = parse_json(&run(&[
        "patch",
        "preview",
        path(&db),
        "--json",
        path(&patch),
    ]));
    assert_eq!(preview["status"], "planned");
    assert_eq!(preview["planned_operations"][0]["kind"], "add_parameter");
    assert_eq!(preview["typecheck"]["status"], "ok");

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        applied["semantic_summary"]["operation_kinds"],
        json!(["add_parameter"])
    );
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "3");
    let show_inc = parse_json(&run(&["show", path(&db), "inc", "--json"]));
    assert_eq!(show_inc["signature"], "(x: i64, scale: i64) -> i64");
    let show_main = parse_json(&run(&["show", path(&db), "main", "--json"]));
    assert_eq!(show_main["body_source"], "inc(2, 1)");
}

#[test]
fn semantic_patch_inline_preserves_caller_locals_without_capture() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("inline-locals.cdb");
    std::fs::write(
        &source,
        "fn add_one(x: i64) -> i64 = let y: i64 = 1 in x + y\n\
         \n\
         fn main() -> i64 = let y: i64 = 7 in add_one(y)\n",
    )
    .unwrap();
    let db = temp.path().join("inline-locals.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "8");

    let root = current_root(&db);
    let inline_patch = write_patch(
        temp.path(),
        "inline-local.patch.json",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": {
                "kind": "call",
                "target_name": "add_one",
                "within_name": "main"
            },
            "replace": {
                "kind": "inline_function"
            }
        }),
    );

    let applied = parse_json(&run(&[
        "patch",
        "apply",
        path(&db),
        "--json",
        path(&inline_patch),
    ]));
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        applied["semantic_summary"]["operation_kinds"],
        json!(["replace_function_body"])
    );
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "8");
    let show_main = parse_json(&run(&["show", path(&db), "main", "--json"]));
    let body_source = show_main["body_source"].as_str().unwrap();
    assert!(body_source.contains("let y: i64 = 7 in"));
    assert!(body_source.contains("in y +"));
}

#[test]
fn semantic_patch_apply_type_error_rolls_back_everything() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("literal-type-error.sqlite");
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

    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "error");
    assert_eq!(applied["committed"], false);
    assert_eq!(applied["typecheck"]["status"], "error");
    assert_eq!(applied["apply_result"]["rollback_reason"], "error");
    assert_eq!(branch_state(&db), branch_before);
    assert_eq!(mutation_counts(&db), counts_before);
    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    assert_eq!(show_tax["body_source"], "subtotal * 20 / 100");
}

#[test]
fn workspace_patch_apply_commits_and_reports_stale_root_conflict() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-patch-apply.sqlite");
    let mut db = CodeDb::open(&db_path).unwrap();
    db.init().unwrap();
    db.import_file(Path::new("examples/shop.cdb")).unwrap();

    let before = branch_state(&db_path);
    let applied = workspace_call(
        &mut db,
        "patch.apply",
        json!({
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
    );
    assert_eq!(applied["schema"], "codedb/response/v1");
    assert_eq!(applied["status"], "ok");
    assert_eq!(
        applied["result"]["schema"],
        "codedb/semantic-patch-apply-result/v1"
    );
    assert_eq!(applied["result"]["status"], "applied");
    assert_eq!(applied["result"]["committed"], true);
    assert_eq!(
        applied["snapshot"]["root_hash"],
        applied["result"]["new_root_hash"]
    );

    let after_patch = branch_state(&db_path);
    let stale_no_match = workspace_call(
        &mut db,
        "patch.apply",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": before.0,
            "match": {
                "kind": "literal_i64",
                "value": "18",
                "within_name": "tax"
            },
            "replace": {
                "kind": "literal_i64",
                "value": "17"
            }
        }),
    );
    assert_eq!(stale_no_match["status"], "error");
    assert_eq!(stale_no_match["error"]["kind"], "stale_root");
    assert_eq!(stale_no_match["error"]["expected_root_hash"], before.0);
    assert_eq!(stale_no_match["error"]["actual_root_hash"], after_patch.0);
    assert_eq!(branch_state(&db_path), after_patch);

    let stale = workspace_call(
        &mut db,
        "patch.apply",
        json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": before.0,
            "match": {
                "kind": "symbol",
                "name": "tax"
            },
            "replace": {
                "kind": "rename_symbol",
                "new_name": "vat"
            }
        }),
    );
    assert_eq!(stale["status"], "error");
    assert_eq!(stale["error"]["kind"], "stale_root");
    assert_eq!(stale["error"]["expected_root_hash"], before.0);
    assert_eq!(stale["error"]["actual_root_hash"], after_patch.0);
    assert_eq!(branch_state(&db_path), after_patch);
}
