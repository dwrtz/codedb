use std::path::Path;

use assert_cmd::Command;
use rusqlite::Connection;
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

fn row_count(db: &Path, table: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn branch(db: &Path, name: &str) -> JsonValue {
    let branches = parse_json(&run(&["branch", "list", path(db), "--json"]));
    branches["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|branch| branch["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("missing branch {name} in {branches}"))
}

#[test]
fn raw_root_branch_fast_forward_requires_target_history() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("root-fast-forward.sqlite");
    let detached_apply_path = temp.path().join("detached-root-branch.apply.json");
    let replayable_apply_path = temp.path().join("replayable-root-branch.apply.json");
    let history_path = temp.path().join("history.ndjson");
    let imported_db = temp.path().join("imported.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let main_before = branch(&db, "main");
    let old_main_root = main_before["root_hash"].as_str().unwrap().to_string();
    let old_main_history = main_before["history_hash"]
        .as_str()
        .expect("main history")
        .to_string();

    let created = parse_json(&run(&[
        "branch",
        "create",
        path(&db),
        "agent/root-work",
        "--from-root",
        &old_main_root,
        "--json",
    ]));
    assert_eq!(created["status"], "created");
    assert_eq!(created["history_hash"], JsonValue::Null);

    std::fs::write(
        &detached_apply_path,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "branch": "agent/root-work",
            "expect_root_hash": old_main_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    run(&["apply", path(&db), "--json", path(&detached_apply_path)]);

    let non_fast_forward = parse_json(&run(&[
        "branch",
        "fast-forward",
        path(&db),
        "main",
        "agent/root-work",
        "--expect-root",
        &old_main_root,
        "--json",
    ]));
    assert_eq!(non_fast_forward["status"], "non_fast_forward");
    assert_eq!(branch(&db, "main")["root_hash"], old_main_root);
    run(&["verify", path(&db)]);

    let replayable = parse_json(&run(&[
        "branch",
        "create",
        path(&db),
        "agent/root-work-with-history",
        "--from-root",
        &old_main_root,
        "--from-history",
        &old_main_history,
        "--json",
    ]));
    assert_eq!(replayable["status"], "created");
    assert_eq!(replayable["history_hash"], old_main_history);

    std::fs::write(
        &replayable_apply_path,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "branch": "agent/root-work-with-history",
            "expect_root_hash": old_main_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let applied = parse_json(&run(&[
        "apply",
        path(&db),
        "--json",
        path(&replayable_apply_path),
    ]));
    let new_root = applied["new_root_hash"].as_str().unwrap().to_string();

    let fast_forwarded = parse_json(&run(&[
        "branch",
        "fast-forward",
        path(&db),
        "main",
        "agent/root-work-with-history",
        "--expect-root",
        &old_main_root,
        "--json",
    ]));
    assert_eq!(fast_forwarded["status"], "fast_forwarded");
    assert_eq!(fast_forwarded["old_root_hash"], old_main_root);
    assert_eq!(fast_forwarded["new_root_hash"], new_root);
    assert_eq!(branch(&db, "main")["root_hash"], new_root);
    run(&["verify", path(&db)]);

    run(&[
        "export-history",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&history_path),
    ]);
    run(&["init", path(&imported_db)]);
    run(&["import-history", path(&imported_db), path(&history_path)]);
    assert_eq!(branch(&imported_db, "main"), branch(&db, "main"));
}

#[test]
fn branch_cli_operations_isolate_writes_and_fast_forward_by_expected_root() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("branches.sqlite");
    let apply_path = temp.path().join("agent-rename.apply.json");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let main_before = branch(&db, "main");
    let old_main_root = main_before["root_hash"].as_str().unwrap().to_string();
    let old_main_history = main_before["history_hash"].clone();
    let objects_before_create = row_count(&db, "objects");
    let migrations_before_create = row_count(&db, "migrations");
    let histories_before_create = row_count(&db, "histories");
    let branches_before_create = row_count(&db, "branches");

    let created = parse_json(&run(&[
        "branch",
        "create",
        path(&db),
        "agent/demo",
        "--from",
        "main",
        "--json",
    ]));
    assert_eq!(created["schema"], "codedb/branch-operation-result/v1");
    assert_eq!(created["status"], "created");
    assert_eq!(created["root_hash"], old_main_root);
    assert_eq!(created["history_hash"], old_main_history);
    assert_eq!(created["source"]["kind"], "branch");
    assert_eq!(created["source"]["branch"], "main");

    let root_created = parse_json(&run(&[
        "branch",
        "create",
        path(&db),
        "agent/root-snapshot",
        "--from-root",
        &old_main_root,
        "--json",
    ]));
    assert_eq!(root_created["status"], "created");
    assert_eq!(root_created["root_hash"], old_main_root);
    assert_eq!(root_created["history_hash"], JsonValue::Null);
    assert_eq!(root_created["source"]["kind"], "root");

    assert_eq!(row_count(&db, "objects"), objects_before_create);
    assert_eq!(row_count(&db, "migrations"), migrations_before_create);
    assert_eq!(row_count(&db, "histories"), histories_before_create);
    assert_eq!(row_count(&db, "branches"), branches_before_create + 2);

    let objects_before_root_delete = row_count(&db, "objects");
    let deleted_root_branch = parse_json(&run(&[
        "branch",
        "delete",
        path(&db),
        "agent/root-snapshot",
        "--json",
    ]));
    assert_eq!(deleted_root_branch["status"], "deleted");
    assert_eq!(row_count(&db, "objects"), objects_before_root_delete);

    std::fs::write(
        &apply_path,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "branch": "agent/demo",
            "expect_root_hash": old_main_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let applied = parse_json(&run(&["apply", path(&db), "--json", path(&apply_path)]));
    assert_eq!(applied["schema"], "codedb/apply-result/v1");
    assert_eq!(applied["branch"], "agent/demo");
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["committed"], true);
    assert_eq!(applied["old_root_hash"], old_main_root);
    assert_ne!(applied["new_root_hash"], old_main_root);

    let main_after_agent_write = branch(&db, "main");
    let agent_after_write = branch(&db, "agent/demo");
    let new_agent_root = agent_after_write["root_hash"].as_str().unwrap().to_string();
    assert_eq!(main_after_agent_write["root_hash"], old_main_root);
    assert_eq!(main_after_agent_write["history_hash"], old_main_history);
    assert_eq!(agent_after_write["root_hash"], applied["new_root_hash"]);

    let compared = parse_json(&run(&[
        "branch",
        "compare",
        path(&db),
        "main",
        "agent/demo",
        "--json",
    ]));
    assert_eq!(compared["schema"], "codedb/branch-compare/v1");
    assert_eq!(compared["branch_a"]["root_hash"], old_main_root);
    assert_eq!(compared["branch_b"]["root_hash"], new_agent_root);
    assert_eq!(compared["same_root"], false);
    assert_eq!(compared["changes"][0]["kind"], "symbol_renamed");

    let fast_forwarded = parse_json(&run(&[
        "branch",
        "fast-forward",
        path(&db),
        "main",
        "agent/demo",
        "--expect-root",
        &old_main_root,
        "--json",
    ]));
    assert_eq!(fast_forwarded["status"], "fast_forwarded");
    assert_eq!(fast_forwarded["old_root_hash"], old_main_root);
    assert_eq!(fast_forwarded["new_root_hash"], new_agent_root);
    assert_eq!(branch(&db, "main")["root_hash"], new_agent_root);

    let stale_fast_forward = parse_json(&run(&[
        "branch",
        "fast-forward",
        path(&db),
        "main",
        "agent/demo",
        "--expect-root",
        &old_main_root,
        "--json",
    ]));
    assert_eq!(stale_fast_forward["status"], "stale_root");
    assert_eq!(stale_fast_forward["expected_root_hash"], old_main_root);
    assert_eq!(stale_fast_forward["actual_root_hash"], new_agent_root);
    assert_eq!(branch(&db, "main")["root_hash"], new_agent_root);

    run(&[
        "branch",
        "create",
        path(&db),
        "agent/sibling-a",
        "--from",
        "main",
    ]);
    run(&[
        "branch",
        "create",
        path(&db),
        "agent/sibling-b",
        "--from",
        "main",
    ]);
    let sibling_base_root = branch(&db, "main")["root_hash"]
        .as_str()
        .unwrap()
        .to_string();

    std::fs::write(
        &apply_path,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "branch": "agent/sibling-a",
            "expect_root_hash": sibling_base_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "vat",
                    "new_name": "tax_a"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let sibling_a = parse_json(&run(&["apply", path(&db), "--json", path(&apply_path)]));
    let sibling_a_root = sibling_a["new_root_hash"].as_str().unwrap().to_string();

    std::fs::write(
        &apply_path,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "branch": "agent/sibling-b",
            "expect_root_hash": sibling_base_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "vat",
                    "new_name": "tax_b"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let sibling_b = parse_json(&run(&["apply", path(&db), "--json", path(&apply_path)]));
    let sibling_b_root = sibling_b["new_root_hash"].as_str().unwrap().to_string();

    let sibling_a_ff = parse_json(&run(&[
        "branch",
        "fast-forward",
        path(&db),
        "main",
        "agent/sibling-a",
        "--expect-root",
        &sibling_base_root,
        "--json",
    ]));
    assert_eq!(sibling_a_ff["status"], "fast_forwarded");
    assert_eq!(branch(&db, "main")["root_hash"], sibling_a_root);

    let non_fast_forward = parse_json(&run(&[
        "branch",
        "fast-forward",
        path(&db),
        "main",
        "agent/sibling-b",
        "--expect-root",
        &sibling_a_root,
        "--json",
    ]));
    assert_eq!(non_fast_forward["status"], "non_fast_forward");
    assert_eq!(non_fast_forward["current_root_hash"], sibling_a_root);
    assert_eq!(
        non_fast_forward["source"]["root_hash"],
        JsonValue::String(sibling_b_root)
    );
    assert_eq!(branch(&db, "main")["root_hash"], sibling_a_root);

    let objects_before_agent_delete = row_count(&db, "objects");
    let deleted = parse_json(&run(&[
        "branch",
        "delete",
        path(&db),
        "agent/demo",
        "--json",
    ]));
    assert_eq!(deleted["status"], "deleted");
    assert_eq!(row_count(&db, "objects"), objects_before_agent_delete);
    let branches = parse_json(&run(&["branch", "list", path(&db), "--json"]));
    assert!(
        branches["branches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|branch| branch["name"] != "agent/demo")
    );

    bin()
        .args(["verify", path(&db)])
        .assert()
        .success()
        .stdout("verify ok\n");
}
