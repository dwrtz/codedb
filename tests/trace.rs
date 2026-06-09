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

fn events(trace: &JsonValue) -> &[JsonValue] {
    trace["events"].as_array().expect("events array")
}

fn event_names(trace: &JsonValue) -> Vec<&str> {
    events(trace)
        .iter()
        .map(|event| event["event"].as_str().expect("event name"))
        .collect()
}

fn call_event_for_symbol<'a>(trace: &'a JsonValue, symbol_hash: &str) -> &'a JsonValue {
    events(trace)
        .iter()
        .find(|event| {
            event["event"] == "call" && event["callee_symbol_hash"].as_str() == Some(symbol_hash)
        })
        .unwrap_or_else(|| panic!("missing call event for {symbol_hash}"))
}

fn enter_event_for_symbol<'a>(trace: &'a JsonValue, symbol_hash: &str) -> &'a JsonValue {
    events(trace)
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
fn trace_cli_returns_deterministic_semantic_events() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("trace.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let first_text = run(&["trace", path(&db), "main", "--json"]);
    let second_text = run(&["trace", path(&db), "main", "--json"]);
    assert_eq!(first_text, second_text);

    let trace = parse_json(&first_text);
    assert_eq!(trace["schema"], "codedb/trace/v1");
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["entry_name"], "main.main");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "120"}));

    let names = event_names(&trace);
    for expected in [
        "enter_function",
        "eval_expr",
        "call",
        "value",
        "exit_function",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }

    let eval_events = events(&trace)
        .iter()
        .filter(|event| event["event"] == "eval_expr")
        .collect::<Vec<_>>();
    assert!(!eval_events.is_empty());
    for event in eval_events {
        assert!(
            event["symbol_hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(
            event["function_def_hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(event["expr_hash"].as_str().unwrap().starts_with("sha256:"));
        assert!(event["type_hash"].as_str().unwrap().starts_with("sha256:"));
    }
}

#[test]
fn trace_steps_through_a_scalar_case() {
    // `trace`/`debug` must step through an i64/bool `case` (R14), not only an enum
    // case: the scalar scrutinee selects an arm by literal pattern with a `_`
    // fallback, like the reference evaluator. Previously this trapped with
    // "case expression evaluated to non-enum".
    let temp = tempdir().unwrap();
    let db = temp.path().join("scalar_case_trace.sqlite");
    let src = temp.path().join("scalar_case_trace.cdb");
    std::fs::write(
        &src,
        "fn classify(n: i64) -> i64 = case n of 0 => 100 | 1 => 200 | _ => 999\n\
         fn main() -> i64 = classify(1)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "200"}));

    let decision = events(&trace)
        .iter()
        .find(|event| event["event"] == "case_decision")
        .expect("a case_decision event for the scalar case");
    assert_eq!(decision["selected_variant"], "1", "selected the `1 => 200` arm");
}

#[test]
fn trace_targets_survive_rename_while_display_names_change() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("trace-rename.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    let tax_symbol = show_tax["symbol_hash"].as_str().unwrap();
    let before = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let before_tax_call = call_event_for_symbol(&before, tax_symbol);
    let before_tax_enter = enter_event_for_symbol(&before, tax_symbol);
    assert_eq!(before_tax_call["callee_name"], "main.tax");

    run(&["rename", path(&db), "tax", "vat"]);
    let after = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let after_tax_call = call_event_for_symbol(&after, tax_symbol);
    let after_tax_enter = enter_event_for_symbol(&after, tax_symbol);

    assert_eq!(after["result"], json!({"kind": "i64", "value": "120"}));
    assert_eq!(after_tax_call["callee_name"], "main.vat");
    assert_eq!(
        before_tax_call["callee_symbol_hash"],
        after_tax_call["callee_symbol_hash"]
    );
    assert_eq!(before_tax_call["expr_hash"], after_tax_call["expr_hash"]);
    assert_eq!(
        before_tax_enter["function_def_hash"],
        after_tax_enter["function_def_hash"]
    );
}

#[test]
fn trace_changes_when_function_body_changes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("trace-body.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    let show_tax = parse_json(&run(&["show", path(&db), "tax", "--json"]));
    let tax_symbol = show_tax["symbol_hash"].as_str().unwrap();
    let before = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let before_tax_enter = enter_event_for_symbol(&before, tax_symbol);

    run(&["replace-body", path(&db), "tax", "subtotal * 18 / 100"]);
    let after = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    let after_tax_enter = enter_event_for_symbol(&after, tax_symbol);

    assert_eq!(after["result"], json!({"kind": "i64", "value": "118"}));
    assert_ne!(
        before_tax_enter["function_def_hash"],
        after_tax_enter["function_def_hash"]
    );
    assert!(events(&after).iter().any(|event| {
        event["event"] == "value" && event["value"] == json!({"kind": "i64", "value": "18"})
    }));
}

#[test]
fn trace_failures_include_structured_diagnostics() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("trace-trap.sqlite");
    let source = temp.path().join("trap.cdb");
    std::fs::write(&source, "fn main() -> i64 = 1 / 0\n").unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "error");
    assert_eq!(trace["result"], JsonValue::Null);
    assert_eq!(trace["diagnostics"][0]["kind"], "trap");
    assert!(
        trace["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("division by zero")
    );
    assert!(
        trace["diagnostics"][0]["location"]["expr_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(events(&trace).iter().any(|event| event["event"] == "trap"));
}

#[test]
fn trace_reports_semantic_places_for_borrows_and_field_access() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("trace-v2-places.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/v2/line_view_refs.cdb"]);

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");

    let borrow = events(&trace)
        .iter()
        .find(|event| event["event"] == "borrow_shared")
        .expect("borrow_shared event");
    assert_eq!(borrow["place"]["root"], "local");
    assert_eq!(borrow["place"]["index"], 0);
    assert_eq!(borrow["place"]["path"], json!([]));
    assert!(borrow["region"].as_str().unwrap().starts_with("sha256:"));
    assert!(
        borrow["referent_type_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );

    let price_field = events(&trace)
        .iter()
        .find(|event| {
            event["event"] == "field_access"
                && event["place"]["path"] == json!(["line", "price_cents"])
        })
        .expect("price_cents field access event");
    assert_eq!(price_field["place"]["root"], "param");
    assert_eq!(price_field["place"]["index"], 0);
    assert_eq!(price_field["field"], "price_cents");
}

#[test]
fn workspace_api_runs_semantic_trace() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("trace-api.sqlite");
    let mut db = CodeDb::open(&db_path).unwrap();
    db.init().unwrap();
    db.import_file(Path::new("examples/shop.cdb")).unwrap();

    let response = workspace_call(&mut db, "trace.run", json!({"entry": "main", "args": []}));
    assert_eq!(response["schema"], "codedb/response/v1");
    assert_eq!(response["status"], "ok");
    assert_eq!(response["result"]["schema"], "codedb/trace/v1");
    assert_eq!(response["result"]["status"], "ok");
    assert_eq!(
        response["result"]["result"],
        json!({"kind": "i64", "value": "120"})
    );
    assert_eq!(
        response["snapshot"]["root_hash"],
        response["result"]["root_hash"]
    );
}

#[test]
fn workspace_api_returns_error_envelope_for_trace_failures() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("trace-api-trap.sqlite");
    let source = temp.path().join("trap.cdb");
    std::fs::write(&source, "fn main() -> i64 = 1 / 0\n").unwrap();
    let mut db = CodeDb::open(&db_path).unwrap();
    db.init().unwrap();
    db.import_file(&source).unwrap();

    let response = workspace_call(&mut db, "trace.run", json!({"entry": "main", "args": []}));
    assert_eq!(response["schema"], "codedb/response/v1");
    assert_eq!(response["status"], "error");
    assert_eq!(response["error"]["kind"], "trace_error");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("division by zero")
    );
    assert_eq!(response["diagnostics"][0]["kind"], "trap");
    assert_eq!(
        response["diagnostics"][0]["details"]["location"]["root_hash"],
        response["snapshot"]["root_hash"]
    );
    assert_eq!(response["result"], JsonValue::Null);
}
