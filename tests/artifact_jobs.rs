use std::path::Path;
use std::process::{Command as StdCommand, Stdio};

use assert_cmd::Command;
use codedb::CodeDb;
use codedb::workspace::{WorkspaceRequest, WorkspaceResponse, execute_workspace_request};
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

fn workspace_call(db: &mut CodeDb, method: &str, params: JsonValue) -> JsonValue {
    let response: WorkspaceResponse = execute_workspace_request(
        db,
        WorkspaceRequest {
            schema: None,
            jsonrpc: Some("2.0".to_string()),
            method: method.to_string(),
            params,
            id: None,
            request_id: None,
        },
    );
    serde_json::to_value(response).unwrap()
}

fn job_count_by_kind(db: &Path, artifact_kind: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM artifact_jobs WHERE artifact_kind = ?1",
        [artifact_kind],
        |row| row.get(0),
    )
    .unwrap()
}

fn cache_count_by_kind(db: &Path, artifact_kind: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM compile_cache WHERE artifact_kind = ?1",
        [artifact_kind],
        |row| row.get(0),
    )
    .unwrap()
}

fn job_statuses(db: &Path, artifact_kind: &str) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT status FROM artifact_jobs
             WHERE artifact_kind = ?1
             ORDER BY cache_key",
        )
        .unwrap();
    stmt.query_map([artifact_kind], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn failed_job_error(db: &Path) -> (String, JsonValue) {
    let conn = Connection::open(db).unwrap();
    let (worker_id, error_json): (String, String) = conn
        .query_row(
            "SELECT worker_id, error_json FROM artifact_jobs
             WHERE status = 'failed'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    (worker_id, serde_json::from_str(&error_json).unwrap())
}

#[test]
fn concurrent_build_plan_workers_share_artifact_jobs_without_materializing_cache_rows() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("concurrent-artifacts.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let bin = assert_cmd::cargo::cargo_bin("codedb");
    let args = [
        "build-plan",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--json",
    ];
    let first = StdCommand::new(&bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first build-plan");
    let second = StdCommand::new(&bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn second build-plan");

    let first_output = first.wait_with_output().expect("first output");
    let second_output = second.wait_with_output().expect("second output");
    assert!(
        first_output.status.success(),
        "first failed:\n{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert!(
        second_output.status.success(),
        "second failed:\n{}",
        String::from_utf8_lossy(&second_output.stderr)
    );

    let first_plan = parse_json(&String::from_utf8(first_output.stdout).unwrap());
    let second_plan = parse_json(&String::from_utf8(second_output.stdout).unwrap());
    assert_eq!(
        first_plan["link_plan_cache_key"],
        second_plan["link_plan_cache_key"]
    );
    assert_eq!(first_plan["link_plan_hash"], JsonValue::Null);
    assert_eq!(second_plan["link_plan_hash"], JsonValue::Null);
    assert_eq!(first_plan["jobs"].as_array().unwrap().len(), 4);
    assert_eq!(second_plan["jobs"].as_array().unwrap().len(), 4);
    assert_eq!(job_count_by_kind(&db, "object_file"), 3);
    assert_eq!(job_count_by_kind(&db, "link_plan"), 1);
    assert_eq!(cache_count_by_kind(&db, "object_file"), 0);
    assert_eq!(cache_count_by_kind(&db, "link_plan"), 0);
    assert!(
        job_statuses(&db, "object_file")
            .iter()
            .all(|status| status == "queued")
    );
    assert_eq!(job_statuses(&db, "link_plan"), vec!["queued"]);
}

#[test]
fn failed_object_jobs_record_structured_errors_and_retry() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("failed-artifact.sqlite");
    let source = temp.path().join("too-many-params.cdb");
    std::fs::write(
        &source,
        "fn main(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64) -> i64 = a\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let first_assert = bin()
        .args([
            "link-native",
            path(&db),
            "main",
            "--target",
            codedb::LINUX_X86_64_TARGET,
            "--out",
            path(&temp.path().join("too-many-params.link.json")),
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8(first_assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("at most 6 parameters"));
    let (first_worker, error) = failed_job_error(&db);
    assert_eq!(error["schema"], "codedb/artifact-job-error/v1");
    assert_eq!(error["kind"], "compile_failed");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("at most 6 parameters")
    );

    bin()
        .args([
            "link-native",
            path(&db),
            "main",
            "--target",
            codedb::LINUX_X86_64_TARGET,
            "--out",
            path(&temp.path().join("too-many-params-retry.link.json")),
        ])
        .assert()
        .failure();
    let (second_worker, retry_error) = failed_job_error(&db);
    assert_ne!(first_worker, second_worker);
    assert_eq!(retry_error["kind"], "compile_failed");
}

#[test]
fn succeeded_jobs_retry_after_disposable_cache_entries_are_deleted() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("disposable-cache.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    run(&[
        "link-native",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&temp.path().join("main.link.json")),
    ]);
    assert_eq!(cache_count_by_kind(&db, "object_file"), 3);
    assert_eq!(cache_count_by_kind(&db, "link_plan"), 1);
    assert!(
        job_statuses(&db, "object_file")
            .iter()
            .all(|status| status == "succeeded")
    );
    assert_eq!(job_statuses(&db, "link_plan"), vec!["succeeded"]);

    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM compile_cache", []).unwrap();
    drop(conn);
    assert_eq!(cache_count_by_kind(&db, "object_file"), 0);
    assert_eq!(cache_count_by_kind(&db, "link_plan"), 0);
    run(&["verify", path(&db)]);

    run(&[
        "link-native",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&temp.path().join("main-rebuilt.link.json")),
    ]);
    assert_eq!(cache_count_by_kind(&db, "object_file"), 3);
    assert_eq!(cache_count_by_kind(&db, "link_plan"), 1);
    run(&["verify", path(&db)]);
}

#[test]
fn rename_reuses_object_jobs_and_body_change_enqueues_changed_object_and_link_work() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("artifact-impact.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    run(&[
        "build-plan",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--json",
    ]);
    assert_eq!(job_count_by_kind(&db, "object_file"), 3);
    assert_eq!(job_count_by_kind(&db, "link_plan"), 1);

    run(&["rename", path(&db), "tax", "vat"]);
    run(&[
        "build-plan",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--json",
    ]);
    assert_eq!(job_count_by_kind(&db, "object_file"), 3);
    assert_eq!(job_count_by_kind(&db, "link_plan"), 1);

    run(&["replace-body", path(&db), "vat", "subtotal * 25 / 100"]);
    run(&[
        "build-plan",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--json",
    ]);
    assert_eq!(job_count_by_kind(&db, "object_file"), 4);
    assert_eq!(job_count_by_kind(&db, "link_plan"), 2);
    assert_eq!(cache_count_by_kind(&db, "object_file"), 0);
    assert_eq!(cache_count_by_kind(&db, "link_plan"), 0);
}

#[test]
fn workspace_artifact_status_reports_jobs_and_cache_entries() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-artifact-status.sqlite");
    let mut db = CodeDb::open(&db_path).unwrap();
    db.init().unwrap();
    db.import_file(Path::new("examples/shop.cdb")).unwrap();

    let plan = workspace_call(
        &mut db,
        "build.plan",
        json!({"entry_name": "main", "target": codedb::LINUX_X86_64_TARGET}),
    );
    assert_eq!(plan["status"], "ok");
    assert_eq!(plan["result"]["jobs"].as_array().unwrap().len(), 4);

    let status = workspace_call(&mut db, "build.artifact_status", json!({}));
    assert_eq!(status["status"], "ok");
    assert_eq!(status["result"]["schema"], "codedb/artifact-status/v1");
    assert_eq!(status["result"]["jobs"].as_array().unwrap().len(), 4);
    assert!(
        status["result"]["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|job| job["status"] == "queued")
    );
    assert!(
        status["result"]["cache_entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["artifact_kind"] != "object_file")
    );
}

#[test]
fn workspace_build_execute_validates_entry_signature_on_requested_branch() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("workspace-branch-build.sqlite");
    let source = temp.path().join("main.cdb");
    std::fs::write(&source, "fn main() -> i64 = 1\n").unwrap();

    let mut db = CodeDb::open(&db_path).unwrap();
    db.init().unwrap();
    db.import_file(&source).unwrap();

    let current = workspace_call(&mut db, "workspace.current", json!({}));
    let root = current["snapshot"]["root_hash"].as_str().unwrap();
    let created = workspace_call(
        &mut db,
        "workspace.branch.create",
        json!({"name": "agent/params", "from_branch": "main"}),
    );
    assert_eq!(created["status"], "ok");

    let changed = workspace_call(
        &mut db,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "agent/params",
            "expect_root_hash": root,
            "operations": [
                {
                    "kind": "change_function_signature",
                    "name": "main",
                    "params": [{"name": "x", "type": "i64"}],
                    "return_type": "i64"
                }
            ]
        }),
    );
    assert_eq!(changed["status"], "ok");

    let build = workspace_call(
        &mut db,
        "build.execute",
        json!({
            "branch": "agent/params",
            "entry": "main",
            "target": codedb::DEFAULT_NATIVE_TARGET
        }),
    );
    assert_eq!(build["status"], "error");
    assert_eq!(build["error"]["kind"], "method_error");
    assert!(
        build["error"]["message"]
            .as_str()
            .unwrap()
            .contains("native executable entry must not take parameters")
    );
    assert_eq!(build["snapshot"]["branch"], "agent/params");
}
