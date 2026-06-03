use std::path::Path;

use assert_cmd::Command;
use codedb::CodeDb;
use codedb::debugger::{DebugCommand, parse_debug_command};
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

fn run_debug(db: &Path, entry: &str, commands: &[String]) -> JsonValue {
    let mut cmd = bin();
    cmd.arg("debug").arg(path(db)).arg(entry).arg("--json");
    for command in commands {
        cmd.arg("--cmd").arg(command);
    }
    let output = cmd.assert().success().get_output().clone();
    parse_json(&String::from_utf8(output.stdout).expect("utf8 stdout"))
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn events(trace: &JsonValue) -> &[JsonValue] {
    trace["events"].as_array().expect("events array")
}

fn command<'a>(report: &'a JsonValue, command: &str) -> &'a JsonValue {
    report["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["command"].as_str() == Some(command))
        .unwrap_or_else(|| panic!("missing command {command} in {report}"))
}

fn show_symbol(db: &Path, name: &str) -> JsonValue {
    parse_json(&run(&["show", path(db), name, "--json"]))
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
fn debug_cli_steps_through_shop_semantic_frames() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("debug-shop.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let mut commands = vec!["where".to_string()];
    commands.extend((0..13).map(|_| "step".to_string()));
    let report = run_debug(&db, "main", &commands);

    assert_eq!(report["schema"], "codedb/debug-session/v1");
    assert_eq!(report["status"], "ok");
    let entered = report["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|record| {
            let current = &record["state"]["current"];
            (current["event"] == "enter_function").then(|| {
                current["function_name"]
                    .as_str()
                    .expect("function_name")
                    .to_string()
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(entered, vec!["main.main", "main.total", "main.tax"]);
}

#[test]
fn debug_symbol_breakpoint_survives_rename() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("debug-rename.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);
    let tax_symbol = show_symbol(&db, "tax")["symbol_hash"]
        .as_str()
        .unwrap()
        .to_string();

    run(&["rename", path(&db), "tax", "vat"]);
    let report = run_debug(
        &db,
        "main",
        &[
            format!("break symbol {tax_symbol}"),
            "continue".to_string(),
            "where".to_string(),
        ],
    );

    assert_eq!(report["breakpoints"][0]["status"], "active");
    assert_eq!(report["breakpoints"][0]["target"], tax_symbol);
    let continued = command(&report, "continue");
    assert_eq!(continued["status"], "hit_breakpoint");
    assert_eq!(continued["state"]["current"]["function_name"], "main.vat");
    assert_eq!(continued["state"]["current"]["symbol_hash"], tax_symbol);
}

#[test]
fn debug_expr_breakpoint_triggers_and_reports_obsolete_after_body_replacement() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("debug-expr.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let tax_symbol = show_symbol(&db, "tax")["symbol_hash"]
        .as_str()
        .unwrap()
        .to_string();
    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let tax_expr = events(&trace)
        .iter()
        .find(|event| {
            event["event"] == "eval_expr"
                && event["symbol_hash"].as_str() == Some(&tax_symbol)
                && event["expr_kind"] == "binary"
        })
        .and_then(|event| event["expr_hash"].as_str())
        .expect("tax binary expression")
        .to_string();

    let before = run_debug(
        &db,
        "main",
        &[format!("break expr {tax_expr}"), "continue".to_string()],
    );
    let continued = command(&before, "continue");
    assert_eq!(continued["status"], "hit_breakpoint");
    assert_eq!(continued["state"]["current"]["expr_hash"], tax_expr);

    run(&["replace-body", path(&db), "tax", "subtotal * 18 / 100"]);
    let after = run_debug(
        &db,
        "main",
        &[format!("break expr {tax_expr}"), "continue".to_string()],
    );
    assert_eq!(after["breakpoints"][0]["kind"], "expr");
    assert_eq!(after["breakpoints"][0]["status"], "obsolete");
    assert_eq!(
        command(&after, &format!("break expr {tax_expr}"))["status"],
        "obsolete_breakpoint"
    );
    assert_eq!(command(&after, "continue")["status"], "completed");
}

#[test]
fn debug_backtrace_params_and_locals_are_semantic() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("debug-locals.sqlite");
    let source = temp.path().join("locals.cdb");
    std::fs::write(
        &source,
        "fn callee(x: i64) -> i64 = let y: i64 = x + 1 in y * 2\nfn main() -> i64 = callee(9)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let local_ref = events(&trace)
        .iter()
        .find(|event| event["event"] == "eval_expr" && event["expr_kind"] == "local_ref")
        .and_then(|event| event["expr_hash"].as_str())
        .expect("local_ref expression")
        .to_string();

    let report = run_debug(
        &db,
        "main",
        &[
            format!("break expr {local_ref}"),
            "continue".to_string(),
            "print params".to_string(),
            "print locals".to_string(),
            "backtrace".to_string(),
            "show expr".to_string(),
            "show function".to_string(),
        ],
    );

    let continued = command(&report, "continue");
    assert_eq!(continued["status"], "hit_breakpoint");
    assert_eq!(continued["state"]["current"]["expr_kind"], "local_ref");

    let params = &command(&report, "print params")["result"]["params"];
    assert_eq!(params[0]["name"], "x");
    assert_eq!(params[0]["type_name"], "i64");
    assert_eq!(params[0]["value"], json!({"kind": "i64", "value": "9"}));

    let locals = &command(&report, "print locals")["result"]["locals"];
    assert_eq!(locals[0]["name"], "y");
    assert_eq!(locals[0]["type_name"], "i64");
    assert_eq!(locals[0]["value"], json!({"kind": "i64", "value": "10"}));

    let frames = &command(&report, "backtrace")["result"]["frames"];
    assert_eq!(frames[0]["function_name"], "main.callee");
    assert_eq!(frames[1]["function_name"], "main.main");
    assert!(
        frames[0]["symbol_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );

    assert_eq!(
        command(&report, "show expr")["result"]["expr"]["expr_hash"],
        local_ref
    );
    assert_eq!(
        command(&report, "show function")["result"]["function"]["name"],
        "callee"
    );
}

#[test]
fn debug_command_parser_has_non_interactive_coverage() {
    assert_eq!(parse_debug_command("step").unwrap(), DebugCommand::Step);
    assert_eq!(parse_debug_command("next").unwrap(), DebugCommand::Next);
    assert_eq!(
        parse_debug_command("break symbol sha256:symbol").unwrap(),
        DebugCommand::BreakSymbol("sha256:symbol".to_string())
    );
    assert_eq!(
        parse_debug_command("show expr sha256:expr").unwrap(),
        DebugCommand::ShowExpr(Some("sha256:expr".to_string()))
    );
    assert!(parse_debug_command("break line 10").is_err());
}

#[test]
fn workspace_api_runs_debugger_commands() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("debug-api.sqlite");
    let mut db = CodeDb::open(&db_path).unwrap();
    db.init().unwrap();
    db.import_file(Path::new("examples/shop.cdb")).unwrap();

    let response = workspace_call(
        &mut db,
        "debug.run",
        json!({
            "entry": "main",
            "args": [],
            "commands": ["where", "step"]
        }),
    );
    assert_eq!(response["schema"], "codedb/response/v1");
    assert_eq!(response["status"], "ok");
    assert_eq!(response["result"]["schema"], "codedb/debug-session/v1");
    assert_eq!(response["result"]["commands"][0]["command"], "where");
    assert_eq!(
        response["snapshot"]["root_hash"],
        response["result"]["root_hash"]
    );
}
