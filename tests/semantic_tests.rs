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

fn run_failure(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn workspace_call(db: &std::path::Path, method: &str, params: JsonValue) -> JsonValue {
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

#[test]
fn semantic_test_objects_run_and_survive_history_export_import() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("semantic-tests.sqlite");
    let rebuilt_db = temp.path().join("semantic-tests-rebuilt.sqlite");
    let history = temp.path().join("history.ndjson");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    let created = parse_json(&run(&[
        "create-test",
        db.to_str().unwrap(),
        "main_returns_120",
        "--entry",
        "main",
        "--expect-i64",
        "120",
        "--native-agreement",
        "--json",
    ]));
    assert_eq!(created["status"], "applied");
    assert_eq!(created["summary"]["kind"], "create_test");
    assert_eq!(created["summary"]["build_impact"]["kind"], "metadata_only");

    let listed = parse_json(&run(&["test", db.to_str().unwrap(), "--list", "--json"]));
    assert_eq!(listed["schema"], "codedb/tests-list/v1");
    assert_eq!(listed["tests"].as_array().unwrap().len(), 1);
    assert_eq!(listed["tests"][0]["name"], "main_returns_120");
    assert_eq!(listed["tests"][0]["entry_name"], "main");
    assert_eq!(
        listed["tests"][0]["expected"],
        json!({"kind": "i64", "value": "120"})
    );

    let run_report = parse_json(&run(&["test", db.to_str().unwrap(), "--json"]));
    assert_eq!(run_report["schema"], "codedb/test-run/v1");
    assert_eq!(run_report["status"], "passed");
    assert_eq!(run_report["passed"], 1);
    assert_eq!(run_report["tests"][0]["status"], "passed");
    assert_eq!(
        run_report["tests"][0]["reference"]["actual"],
        json!({"kind": "i64", "value": "120"})
    );
    assert!(
        ["passed", "skipped"].contains(
            &run_report["tests"][0]["native_agreement"]["status"]
                .as_str()
                .unwrap()
        ),
        "unexpected native agreement status: {}",
        run_report["tests"][0]["native_agreement"]
    );

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");

    run(&[
        "export-history",
        db.to_str().unwrap(),
        "--out",
        history.to_str().unwrap(),
    ]);
    run(&["init", rebuilt_db.to_str().unwrap()]);
    run(&[
        "import-history",
        rebuilt_db.to_str().unwrap(),
        history.to_str().unwrap(),
    ]);

    let rebuilt_listed = parse_json(&run(&[
        "test",
        rebuilt_db.to_str().unwrap(),
        "--list",
        "--json",
    ]));
    assert_eq!(rebuilt_listed, listed);
    let rebuilt_report = parse_json(&run(&["test", rebuilt_db.to_str().unwrap(), "--json"]));
    assert_eq!(rebuilt_report["status"], "passed");
    assert_eq!(rebuilt_report["tests"][0]["reference"]["status"], "passed");
}

#[test]
fn semantic_test_runner_reports_failures_and_rejects_invalid_test_inputs() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("semantic-test-failures.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    run(&[
        "create-test",
        db.to_str().unwrap(),
        "main_returns_120",
        "--entry",
        "main",
        "--expect-i64",
        "120",
    ]);

    run(&[
        "replace-body",
        db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    let report = parse_json(&run(&["test", db.to_str().unwrap(), "--json"]));
    assert_eq!(report["status"], "failed");
    assert_eq!(report["failed"], 1);
    assert_eq!(report["tests"][0]["reference"]["status"], "failed");
    assert_eq!(
        report["tests"][0]["reference"]["actual"],
        json!({"kind": "i64", "value": "118"})
    );

    let bad_arg = run_failure(&[
        "create-test",
        db.to_str().unwrap(),
        "tax_bad_arg",
        "--entry",
        "tax",
        "--arg",
        "not_i64",
        "--expect-i64",
        "0",
    ]);
    assert!(bad_arg.contains("argument 0 must be i64"));

    let bad_expected = run_failure(&[
        "create-test",
        db.to_str().unwrap(),
        "main_bad_expected",
        "--entry",
        "main",
        "--expect-bool",
        "true",
    ]);
    assert!(bad_expected.contains("expected value must be i64"));

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn workspace_api_lists_runs_creates_and_deletes_semantic_tests() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("semantic-tests-workspace.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    let current = workspace_call(&db, "workspace.current", json!({}));
    assert_eq!(current["status"], "ok");
    let root = current["snapshot"]["root_hash"].as_str().unwrap();

    let created = workspace_call(
        &db,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "expect_root_hash": root,
            "operations": [
                {
                    "kind": "create_test",
                    "name": "main_returns_120",
                    "entry": "main",
                    "expected": {"kind": "i64", "value": "120"}
                }
            ]
        }),
    );
    assert_eq!(created["status"], "ok");
    assert_eq!(created["result"]["status"], "applied");
    assert_eq!(
        created["result"]["results"][0]["summary"]["kind"],
        "create_test"
    );
    let root_after_create = created["snapshot"]["root_hash"].as_str().unwrap();

    let listed = workspace_call(&db, "tests.list", json!({}));
    assert_eq!(listed["status"], "ok");
    assert_eq!(listed["result"]["schema"], "codedb/tests-list/v1");
    assert_eq!(listed["result"]["tests"][0]["name"], "main_returns_120");

    let report = workspace_call(&db, "tests.run", json!({}));
    assert_eq!(report["status"], "ok");
    assert_eq!(report["result"]["status"], "passed");
    assert_eq!(
        report["result"]["tests"][0]["reference"]["status"],
        "passed"
    );

    let deleted = workspace_call(
        &db,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "expect_root_hash": root_after_create,
            "operations": [
                {
                    "kind": "delete_test",
                    "name": "main_returns_120"
                }
            ]
        }),
    );
    assert_eq!(deleted["status"], "ok");
    assert_eq!(
        deleted["result"]["results"][0]["summary"]["kind"],
        "delete_test"
    );

    let listed_after_delete = workspace_call(&db, "tests.list", json!({}));
    assert_eq!(listed_after_delete["status"], "ok");
    assert_eq!(
        listed_after_delete["result"]["tests"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}
