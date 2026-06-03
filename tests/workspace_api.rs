use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

struct WorkspaceServer {
    child: Child,
    addr: String,
}

impl Drop for WorkspaceServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(db: &Path) -> WorkspaceServer {
    let addr = free_addr();
    let child = StdCommand::new(assert_cmd::cargo::cargo_bin("codedb"))
        .args(["serve", db.to_str().unwrap(), "--addr", &addr])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codedb serve");
    let mut server = WorkspaceServer { child, addr };
    wait_for_server(&mut server);
    server
}

fn free_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind free port");
    listener.local_addr().expect("local addr").to_string()
}

fn wait_for_server(server: &mut WorkspaceServer) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(&server.addr).is_ok() {
            return;
        }
        if let Some(status) = server.child.try_wait().expect("server status") {
            panic!("codedb serve exited before accepting requests: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for codedb serve"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn workspace_call(server: &WorkspaceServer, method: &str, params: JsonValue) -> JsonValue {
    workspace_call_with_read_timeout(server, method, params, None)
}

fn workspace_call_with_read_timeout(
    server: &WorkspaceServer,
    method: &str,
    params: JsonValue,
    read_timeout: Option<Duration>,
) -> JsonValue {
    let request = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": method,
    });
    let body = serde_json::to_string(&request).expect("request json");
    let mut stream = TcpStream::connect(&server.addr).expect("connect workspace server");
    if let Some(read_timeout) = read_timeout {
        stream
            .set_read_timeout(Some(read_timeout))
            .expect("set read timeout");
    }
    write!(
        stream,
        "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        server.addr,
        body.len(),
        body
    )
    .expect("write request");

    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected HTTP response:\n{response}"
    );
    let (_, body) = response
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("missing HTTP body:\n{response}"));
    serde_json::from_str(body).unwrap_or_else(|err| panic!("invalid response JSON: {err}\n{body}"))
}

#[test]
fn workspace_server_handles_slow_clients_concurrently() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("workspace-concurrent-server.sqlite");
    run(&["init", db.to_str().unwrap()]);
    let server = start_server(&db);

    let slow_request = json!({
        "jsonrpc": "2.0",
        "method": "workspace.current",
        "params": {},
        "id": "slow",
    });
    let slow_body = serde_json::to_string(&slow_request).expect("slow request json");
    let mut slow_stream = TcpStream::connect(&server.addr).expect("connect slow client");
    write!(
        slow_stream,
        "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        server.addr,
        slow_body.len(),
    )
    .expect("write slow request headers");

    let current = workspace_call_with_read_timeout(
        &server,
        "workspace.current",
        json!({}),
        Some(Duration::from_secs(2)),
    );
    assert_eq!(current["schema"], "codedb/response/v1");
    assert_eq!(current["status"], "ok");
    assert_eq!(current["snapshot"]["branch"], "main");

    drop(slow_stream);
}

#[test]
fn workspace_server_exposes_read_only_workspace_methods() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("workspace.sqlite");
    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let server = start_server(&db);

    let current = workspace_call(&server, "workspace.current", json!({}));
    assert_eq!(current["schema"], "codedb/response/v1");
    assert_eq!(current["status"], "ok");
    assert_eq!(current["snapshot"]["branch"], "main");
    assert_eq!(current["result"]["branch"], "main");
    let root_hash = current["snapshot"]["root_hash"].as_str().unwrap();
    assert_eq!(current["result"]["root_hash"], root_hash);

    let branches = workspace_call(&server, "workspace.branches", json!({}));
    assert_eq!(branches["status"], "ok");
    assert_eq!(branches["result"]["schema"], "codedb/branches/v1");
    assert_eq!(branches["result"]["branches"][0]["name"], "main");

    let symbols = workspace_call(&server, "symbols.list", json!({}));
    assert_eq!(symbols["status"], "ok");
    assert_eq!(symbols["result"]["symbols"].as_array().unwrap().len(), 3);
    assert!(
        symbols["result"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "tax")
    );

    let show = workspace_call(&server, "symbols.show", json!({"name": "tax"}));
    assert_eq!(show["status"], "ok");
    assert_eq!(show["result"]["name"], "tax");
    assert_eq!(show["result"]["module"], "main");
    assert_eq!(show["result"]["body_source"], "subtotal * 20 / 100");

    let resolve = workspace_call(&server, "symbols.resolve", json!({"name": "tax"}));
    assert_eq!(resolve["status"], "ok");
    assert_eq!(resolve["result"]["name"], "tax");
    assert_eq!(resolve["result"]["module"], "main");
    assert_eq!(
        resolve["result"]["symbol_hash"],
        show["result"]["symbol_hash"]
    );

    let callers = workspace_call(&server, "symbols.callers", json!({"name": "tax"}));
    assert_eq!(callers["status"], "ok");
    assert_eq!(callers["result"]["callers"][0]["name"], "total");

    let diff = workspace_call(
        &server,
        "roots.diff",
        json!({"root_a": root_hash, "root_b": root_hash}),
    );
    assert_eq!(diff["status"], "ok");
    assert_eq!(diff["result"]["build_impact"]["kind"], "metadata_only");
    assert_eq!(diff["result"]["changes"].as_array().unwrap().len(), 0);

    let projection = workspace_call(&server, "roots.export_projection", json!({}));
    assert_eq!(projection["status"], "ok");
    assert!(
        projection["result"]["source"]
            .as_str()
            .unwrap()
            .contains("fn tax")
    );

    let build_plan = workspace_call(
        &server,
        "build.plan",
        json!({"entry_name": "main", "target": codedb::LINUX_X86_64_TARGET}),
    );
    assert_eq!(build_plan["status"], "ok");
    assert_eq!(
        build_plan["result"]["schema"],
        "codedb/native-build-plan/v1"
    );
    assert_eq!(build_plan["result"]["objects"].as_array().unwrap().len(), 3);

    let history = workspace_call(&server, "history.list", json!({}));
    assert_eq!(history["status"], "ok");
    assert_eq!(history["result"]["branch"], "main");
    assert_eq!(history["result"]["migrations"].as_array().unwrap().len(), 3);

    let verify = workspace_call(&server, "verify.run", json!({}));
    assert_eq!(verify["status"], "ok");
    assert_eq!(verify["result"]["ok"], true);
    assert_eq!(verify["result"]["message"], "verify ok");
}

#[test]
fn workspace_server_manages_branch_pointers() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("workspace-branches.sqlite");
    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let server = start_server(&db);

    let main_before = workspace_call(&server, "workspace.current", json!({}));
    let old_main_root = main_before["snapshot"]["root_hash"].as_str().unwrap();
    let old_main_history = main_before["snapshot"]["history_hash"].clone();

    let created = workspace_call(
        &server,
        "workspace.branch.create",
        json!({"name": "agent/demo", "from_branch": "main"}),
    );
    assert_eq!(created["status"], "ok");
    assert_eq!(
        created["result"]["schema"],
        "codedb/branch-operation-result/v1"
    );
    assert_eq!(created["result"]["status"], "created");
    assert_eq!(created["result"]["branch"], "agent/demo");
    assert_eq!(created["snapshot"]["branch"], "agent/demo");
    assert_eq!(created["snapshot"]["root_hash"], old_main_root);
    assert_eq!(created["snapshot"]["history_hash"], old_main_history);

    let duplicate = workspace_call(
        &server,
        "workspace.branch.create",
        json!({"name": "agent/demo", "from_branch": "main"}),
    );
    assert_eq!(duplicate["status"], "error");
    assert_eq!(duplicate["error"]["kind"], "name_conflict");

    let applied = workspace_call(
        &server,
        "ops.apply",
        json!({
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
        }),
    );
    assert_eq!(applied["status"], "ok");
    assert_eq!(applied["snapshot"]["branch"], "agent/demo");
    let new_agent_root = applied["snapshot"]["root_hash"].as_str().unwrap();
    let new_agent_history = applied["snapshot"]["history_hash"].clone();
    assert_ne!(new_agent_root, old_main_root);

    let agent_symbols = workspace_call(&server, "symbols.list", json!({"branch": "agent/demo"}));
    assert_eq!(agent_symbols["status"], "ok");
    assert_eq!(agent_symbols["snapshot"]["branch"], "agent/demo");
    assert!(
        agent_symbols["result"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "vat")
    );
    assert!(
        agent_symbols["result"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|symbol| symbol["name"] != "tax")
    );

    let agent_show = workspace_call(
        &server,
        "symbols.show",
        json!({"branch": "agent/demo", "name": "vat"}),
    );
    assert_eq!(agent_show["status"], "ok");
    assert_eq!(agent_show["result"]["branch"], "agent/demo");
    assert_eq!(agent_show["result"]["name"], "vat");

    let main_tax = workspace_call(&server, "symbols.show", json!({"name": "tax"}));
    assert_eq!(main_tax["status"], "ok");
    assert_eq!(main_tax["result"]["name"], "tax");
    let main_vat = workspace_call(&server, "symbols.show", json!({"name": "vat"}));
    assert_eq!(main_vat["status"], "error");

    let agent_resolve = workspace_call(
        &server,
        "symbols.resolve",
        json!({"branch": "agent/demo", "name": "vat"}),
    );
    assert_eq!(agent_resolve["status"], "ok");
    assert_eq!(agent_resolve["result"]["branch"], "agent/demo");
    assert_eq!(
        agent_resolve["result"]["symbol_hash"],
        agent_show["result"]["symbol_hash"]
    );

    let agent_callers = workspace_call(
        &server,
        "symbols.callers",
        json!({"branch": "agent/demo", "name": "vat"}),
    );
    assert_eq!(agent_callers["status"], "ok");
    assert_eq!(agent_callers["result"]["branch"], "agent/demo");
    assert_eq!(agent_callers["result"]["callers"][0]["name"], "total");

    let agent_build_plan = workspace_call(
        &server,
        "build.plan",
        json!({
            "branch": "agent/demo",
            "entry_name": "main",
            "target": codedb::LINUX_X86_64_TARGET
        }),
    );
    assert_eq!(agent_build_plan["status"], "ok");
    assert_eq!(agent_build_plan["snapshot"]["branch"], "agent/demo");
    assert_eq!(agent_build_plan["result"]["branch"], "agent/demo");

    let agent_trace = workspace_call(
        &server,
        "trace.run",
        json!({"branch": "agent/demo", "entry": "main", "args": []}),
    );
    assert_eq!(agent_trace["status"], "ok");
    assert_eq!(agent_trace["result"]["branch"], "agent/demo");
    assert_eq!(
        agent_trace["result"]["result"],
        json!({"kind": "i64", "value": "120"})
    );

    let agent_debug = workspace_call(
        &server,
        "debug.run",
        json!({
            "branch": "agent/demo",
            "entry": "main",
            "args": [],
            "commands": ["where"]
        }),
    );
    assert_eq!(agent_debug["status"], "ok");
    assert_eq!(agent_debug["result"]["branch"], "agent/demo");

    let agent_history = workspace_call(&server, "history.list", json!({"branch": "agent/demo"}));
    assert_eq!(agent_history["status"], "ok");
    assert_eq!(agent_history["result"]["branch"], "agent/demo");
    assert_eq!(
        agent_history["result"]["migrations"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    let main_after_agent_write = workspace_call(&server, "workspace.current", json!({}));
    assert_eq!(
        main_after_agent_write["snapshot"]["root_hash"],
        old_main_root
    );
    assert_eq!(
        main_after_agent_write["snapshot"]["history_hash"],
        old_main_history
    );

    let agent_before = workspace_call(
        &server,
        "workspace.current",
        json!({"branch": "agent/demo"}),
    );
    assert_eq!(agent_before["status"], "ok");
    assert_eq!(agent_before["snapshot"]["root_hash"], new_agent_root);

    let compared = workspace_call(
        &server,
        "workspace.branch.compare",
        json!({"branch_a": "main", "branch_b": "agent/demo"}),
    );
    assert_eq!(compared["status"], "ok");
    assert_eq!(compared["result"]["schema"], "codedb/branch-compare/v1");
    assert_eq!(compared["result"]["branch_a"]["root_hash"], old_main_root);
    assert_eq!(compared["result"]["branch_b"]["root_hash"], new_agent_root);
    assert_eq!(compared["result"]["same_root"], false);
    assert_eq!(compared["result"]["changes"][0]["kind"], "symbol_renamed");

    let fast_forwarded = workspace_call(
        &server,
        "workspace.branch.fast_forward",
        json!({
            "branch": "main",
            "source_branch": "agent/demo",
            "expect_root_hash": old_main_root
        }),
    );
    assert_eq!(fast_forwarded["status"], "ok");
    assert_eq!(fast_forwarded["result"]["status"], "fast_forwarded");
    assert_eq!(fast_forwarded["result"]["old_root_hash"], old_main_root);
    assert_eq!(fast_forwarded["result"]["new_root_hash"], new_agent_root);
    assert_eq!(fast_forwarded["snapshot"]["branch"], "main");
    assert_eq!(fast_forwarded["snapshot"]["root_hash"], new_agent_root);
    assert_eq!(
        fast_forwarded["snapshot"]["history_hash"],
        new_agent_history
    );

    let stale_fast_forward = workspace_call(
        &server,
        "workspace.branch.fast_forward",
        json!({
            "branch": "main",
            "source_branch": "agent/demo",
            "expect_root_hash": old_main_root
        }),
    );
    assert_eq!(stale_fast_forward["status"], "error");
    assert_eq!(stale_fast_forward["error"]["kind"], "stale_root");
    assert_eq!(
        stale_fast_forward["error"]["expected_root_hash"],
        old_main_root
    );
    assert_eq!(
        stale_fast_forward["error"]["actual_root_hash"],
        new_agent_root
    );
    assert_eq!(stale_fast_forward["snapshot"]["branch"], "main");
    assert_eq!(stale_fast_forward["snapshot"]["root_hash"], new_agent_root);

    let stale_delete = workspace_call(
        &server,
        "workspace.branch.delete",
        json!({"branch": "agent/demo", "expect_root_hash": old_main_root}),
    );
    assert_eq!(stale_delete["status"], "error");
    assert_eq!(stale_delete["error"]["kind"], "stale_root");
    assert_eq!(stale_delete["error"]["expected_root_hash"], old_main_root);
    assert_eq!(stale_delete["error"]["actual_root_hash"], new_agent_root);
    assert_eq!(stale_delete["snapshot"]["branch"], "agent/demo");

    let deleted = workspace_call(
        &server,
        "workspace.branch.delete",
        json!({"branch": "agent/demo", "expect_root_hash": new_agent_root}),
    );
    assert_eq!(deleted["status"], "ok");
    assert_eq!(deleted["result"]["status"], "deleted");
    assert_eq!(deleted["result"]["branch"], "agent/demo");
    assert_eq!(deleted["snapshot"]["branch"], "main");

    let branches = workspace_call(&server, "workspace.branches", json!({}));
    assert!(
        branches["result"]["branches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|branch| branch["name"] != "agent/demo")
    );

    let delete_main = workspace_call(
        &server,
        "workspace.branch.delete",
        json!({"branch": "main"}),
    );
    assert_eq!(delete_main["status"], "error");
    assert_eq!(delete_main["error"]["kind"], "invalid_params");
}

#[test]
fn workspace_server_applies_structural_operations_atomically() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("workspace-apply.sqlite");
    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let server = start_server(&db);

    let before = workspace_call(&server, "workspace.current", json!({}));
    let old_root = before["snapshot"]["root_hash"].as_str().unwrap();
    let old_history = before["snapshot"]["history_hash"].clone();

    let applied = workspace_call(
        &server,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": old_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                }
            ]
        }),
    );
    assert_eq!(applied["schema"], "codedb/response/v1");
    assert_eq!(applied["status"], "ok");
    assert_eq!(applied["result"]["schema"], "codedb/apply-result/v1");
    assert_eq!(applied["result"]["status"], "applied");
    assert_eq!(applied["result"]["committed"], true);
    assert_eq!(applied["result"]["old_root_hash"], old_root);
    assert_ne!(applied["result"]["new_root_hash"], old_root);
    assert_eq!(
        applied["result"]["operations"],
        applied["result"]["results"]
    );
    assert_eq!(applied["result"]["old_history_hash"], old_history);
    assert_ne!(applied["result"]["new_history_hash"], old_history);
    assert_eq!(
        applied["result"]["history_hash"],
        applied["result"]["new_history_hash"]
    );
    assert_eq!(
        applied["snapshot"]["root_hash"],
        applied["result"]["new_root_hash"]
    );
    assert_eq!(
        applied["snapshot"]["history_hash"],
        applied["result"]["new_history_hash"]
    );
    assert_eq!(
        applied["result"]["results"][0]["summary"]["build_impact"]["kind"],
        "metadata_only"
    );
    assert!(
        applied["result"]["history_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );

    let show_vat = workspace_call(&server, "symbols.show", json!({"name": "vat"}));
    assert_eq!(show_vat["status"], "ok");
    assert_eq!(show_vat["result"]["name"], "vat");

    let current_after_apply = workspace_call(&server, "workspace.current", json!({}));
    let root_after_apply = current_after_apply["snapshot"]["root_hash"].clone();
    let history_after_apply = current_after_apply["snapshot"]["history_hash"].clone();

    let stale_conflict = workspace_call(
        &server,
        "ops.apply",
        json!({
            "apply": {
                "schema": "codedb/apply/v1",
                "branch": "main",
                "expect_root_hash": old_root,
                "operations": [
                    {
                        "kind": "rename_symbol",
                        "name": "tax",
                        "new_name": "gst"
                    }
                ]
            }
        }),
    );
    assert_eq!(stale_conflict["status"], "error");
    assert_eq!(stale_conflict["error"]["kind"], "stale_root");
    assert_eq!(stale_conflict["error"]["expected_root_hash"], old_root);
    assert_eq!(
        stale_conflict["error"]["actual_root_hash"],
        root_after_apply
    );
    assert_eq!(stale_conflict["snapshot"]["root_hash"], root_after_apply);
    assert_eq!(
        stale_conflict["snapshot"]["history_hash"],
        history_after_apply
    );

    let invalid = workspace_call(
        &server,
        "ops.apply",
        json!({
            "schema": "codedb/apply/v1",
            "expect_root_hash": root_after_apply.clone(),
            "operations": [
                {
                    "kind": "not_real",
                    "name": "vat"
                }
            ]
        }),
    );
    assert_eq!(invalid["status"], "error");
    assert_eq!(invalid["error"]["kind"], "invalid_operation");
    assert_eq!(invalid["snapshot"]["root_hash"], root_after_apply);
}

#[test]
fn workspace_server_previews_structural_operations_without_committing() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("workspace-preview.sqlite");
    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let server = start_server(&db);

    let before = workspace_call(&server, "workspace.current", json!({}));
    let old_root = before["snapshot"]["root_hash"].as_str().unwrap();
    let old_history = before["snapshot"]["history_hash"].clone();

    let preview = workspace_call(
        &server,
        "ops.preview",
        json!({
            "schema": "codedb/apply/v1",
            "branch": "main",
            "expect_root_hash": old_root,
            "operations": [
                {
                    "kind": "rename_symbol",
                    "name": "tax",
                    "new_name": "vat"
                }
            ]
        }),
    );
    assert_eq!(preview["schema"], "codedb/response/v1");
    assert_eq!(preview["status"], "ok");
    assert_eq!(preview["snapshot"]["root_hash"], old_root);
    assert_eq!(preview["snapshot"]["history_hash"], old_history);
    assert_eq!(preview["result"]["schema"], "codedb/apply-result/v1");
    assert_eq!(preview["result"]["preview"], true);
    assert_eq!(preview["result"]["would_commit"], true);
    assert_eq!(preview["result"]["committed"], false);
    assert_eq!(preview["result"]["rollback_reason"], "preview");
    assert_eq!(preview["result"]["status"], "applied");
    assert_eq!(preview["result"]["old_root_hash"], old_root);
    assert_ne!(preview["result"]["new_root_hash"], old_root);
    assert_eq!(preview["result"]["old_history_hash"], old_history);
    assert_ne!(preview["result"]["new_history_hash"], old_history);
    assert_eq!(
        preview["result"]["history_hash"],
        preview["result"]["new_history_hash"]
    );
    assert_eq!(preview["result"]["applied_operation_count"], 1);
    assert_eq!(
        preview["result"]["results"][0]["summary"]["build_impact"]["kind"],
        "metadata_only"
    );

    let after = workspace_call(&server, "workspace.current", json!({}));
    assert_eq!(after["snapshot"]["root_hash"], old_root);
    assert_eq!(after["snapshot"]["history_hash"], old_history);

    let show_tax = workspace_call(&server, "symbols.show", json!({"name": "tax"}));
    assert_eq!(show_tax["status"], "ok");
    let show_vat = workspace_call(&server, "symbols.show", json!({"name": "vat"}));
    assert_eq!(show_vat["status"], "error");
    assert_eq!(show_vat["error"]["kind"], "method_error");

    let applied = workspace_call(
        &server,
        "ops.apply",
        json!({
            "apply": {
                "schema": "codedb/apply/v1",
                "branch": "main",
                "expect_root_hash": old_root,
                "operations": [
                    {
                        "kind": "rename_symbol",
                        "name": "tax",
                        "new_name": "vat"
                    }
                ]
            }
        }),
    );
    assert_eq!(applied["status"], "ok");
    assert_eq!(applied["result"]["committed"], true);
    assert_eq!(
        applied["result"]["new_root_hash"],
        preview["result"]["new_root_hash"]
    );
    assert_eq!(
        applied["snapshot"]["root_hash"],
        applied["result"]["new_root_hash"]
    );
}

#[test]
fn workspace_server_returns_stable_error_envelopes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("workspace-errors.sqlite");
    run(&["init", db.to_str().unwrap()]);
    let server = start_server(&db);

    let unknown = workspace_call(&server, "workspace.nope", json!({}));
    assert_eq!(unknown["schema"], "codedb/response/v1");
    assert_eq!(unknown["status"], "error");
    assert_eq!(unknown["error"]["kind"], "unknown_method");
    assert_eq!(unknown["snapshot"]["branch"], "main");

    let invalid_params = workspace_call(&server, "symbols.list", JsonValue::Null);
    assert_eq!(invalid_params["status"], "error");
    assert_eq!(invalid_params["error"]["kind"], "invalid_params");
    assert_eq!(invalid_params["snapshot"]["branch"], "main");
}
