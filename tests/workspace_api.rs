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
    let request = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": method,
    });
    let body = serde_json::to_string(&request).expect("request json");
    let mut stream = TcpStream::connect(&server.addr).expect("connect workspace server");
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

    let invalid_params = workspace_call(&server, "symbols.list", json!({"branch": "agent/demo"}));
    assert_eq!(invalid_params["status"], "error");
    assert_eq!(invalid_params["error"]["kind"], "invalid_params");
    assert_eq!(invalid_params["snapshot"]["branch"], "main");
}
