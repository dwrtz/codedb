use std::path::Path;

use assert_cmd::Command;
use codedb::CodeDb;
use codedb::workspace::{WorkspaceRequest, WorkspaceResponse, execute_workspace_request};
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
fn why_layout_and_drop_cite_type_and_field_history() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("v2-layout-provenance.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/v2/box_heap.cdb"]);

    let layout = parse_json(&run(&[
        "why-layout",
        path(&db),
        "Line",
        "--field",
        "qty",
        "--json",
    ]));
    assert_eq!(layout["schema"], "codedb/why-layout/v1");
    assert_eq!(layout["field_layout"]["name"], "qty");
    assert_eq!(layout["field_layout"]["offset_bytes"], 8);
    assert!(layout["field_layout"]["field_symbol"].as_str().is_some());
    assert!(
        layout["layout_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        layout["layout_cache_key"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(
        layout["field_blame"]["birth_migration"]["operation_kind"],
        "create_type"
    );
    assert_eq!(
        layout["type_blame"]["birth_migration"]["operation_kind"],
        "create_type"
    );

    let mut api_db = CodeDb::open(&db).unwrap();
    let api_layout = workspace_call(
        &mut api_db,
        "provenance.why_layout",
        json!({"type": "Line", "field": "qty"}),
    );
    assert_eq!(api_layout["status"], "ok");
    assert_eq!(api_layout["result"]["field_layout"]["offset_bytes"], 8);

    let layout_text = run(&["why-layout", path(&db), "Line", "--field", "qty"]);
    assert!(layout_text.contains(&format!(
        "layout_hash {}\n",
        layout["layout_hash"].as_str().unwrap()
    )));
    assert!(layout_text.contains(&format!(
        "layout_cache_key {}\n",
        layout["layout_cache_key"].as_str().unwrap()
    )));
    assert!(layout_text.contains(&format!(
        "field_symbol {}\n",
        layout["field_layout"]["field_symbol"].as_str().unwrap()
    )));
    assert!(layout_text.contains("field_offset 8\n"));

    let drop = parse_json(&run(&["why-drop", path(&db), "Node", "--json"]));
    assert_eq!(drop["schema"], "codedb/why-drop/v1");
    assert!(drop["layout_hash"].as_str().unwrap().starts_with("sha256:"));
    assert!(
        drop["layout_cache_key"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(drop["copy_kind"], "move_only");
    assert_eq!(drop["drop_kind"], "needs_drop");
    assert_eq!(drop["contains_box"], true);
    assert_eq!(drop["contains_owned_resource"], true);
    assert!(
        drop["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason["kind"] == "contains_box")
    );
    assert_eq!(
        drop["type_blame"]["birth_migration"]["operation_kind"],
        "create_type"
    );

    let drop_text = run(&["why-drop", path(&db), "Node"]);
    assert!(drop_text.contains(&format!(
        "layout_hash {}\n",
        drop["layout_hash"].as_str().unwrap()
    )));
    assert!(drop_text.contains(&format!(
        "layout_cache_key {}\n",
        drop["layout_cache_key"].as_str().unwrap()
    )));
    assert!(drop_text.contains("copy_kind move_only\n"));
    assert!(drop_text.contains("drop_kind needs_drop\n"));
    assert!(drop_text.contains("contains_box true\n"));
    assert!(drop_text.contains("reason contains_box\n"));
}

#[test]
fn why_borrow_and_move_explain_candidate_body_failures_without_moving_branch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("v2-candidate-provenance.sqlite");
    let source = temp.path().join("candidate.cdb");
    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record Cursor<'a> {
  line: &'a mut Line
}

fn borrow_probe<'a>() -> unit = ()

fn move_probe<'a>() -> i64 = 0
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    let root_before = current_root(&db);

    let borrow_body = "let line: Line = { price_cents: 1, qty: 2 } in \
        let a: &'a mut Line = &'a mut line in \
        let b: &'a mut Line = &'a mut line in \
        ()";
    let borrow = parse_json(&run(&[
        "why-borrow",
        path(&db),
        "borrow_probe",
        "--body",
        borrow_body,
        "--json",
    ]));
    assert_eq!(borrow["schema"], "codedb/why-borrow/v1");
    assert_eq!(borrow["candidate"]["status"], "invalid");
    assert_eq!(borrow["candidate"]["branch_unchanged"], true);
    assert!(
        borrow["candidate"]["classifications"]
            .as_array()
            .unwrap()
            .contains(&json!("exclusive_loan_conflict"))
    );
    assert!(
        borrow["candidate"]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("exclusive loan conflict")
    );

    let move_body = "let line: Line = { price_cents: 1, qty: 2 } in \
        let cursor: Cursor<'a> = { line: &'a mut line } in \
        let moved: Cursor<'a> = cursor in \
        cursor.line.price_cents";
    let moved = parse_json(&run(&[
        "why-move",
        path(&db),
        "move_probe",
        "--body",
        move_body,
        "--json",
    ]));
    assert_eq!(moved["schema"], "codedb/why-move/v1");
    assert_eq!(moved["candidate"]["status"], "invalid");
    assert!(
        moved["candidate"]["classifications"]
            .as_array()
            .unwrap()
            .contains(&json!("use_after_move"))
    );
    assert!(
        moved["candidate"]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("use after move")
    );
    assert_eq!(current_root(&db), root_before);
}

#[test]
fn why_effect_reports_state_alloc_io_and_platform_extern_reachability() {
    let temp = tempdir().unwrap();

    let state_db = temp.path().join("v2-state-effect.sqlite");
    run(&["init", path(&state_db)]);
    run(&["import", path(&state_db), "examples/v2/mutable_cursor.cdb"]);
    let state = parse_json(&run(&["why-effect", path(&state_db), "main", "--json"]));
    assert_eq!(state["schema"], "codedb/why-effect/v1");
    assert!(
        state["declared_effects"]
            .as_array()
            .unwrap()
            .contains(&json!("state"))
    );
    assert!(
        state["required_by"]["state"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason["kind"] == "body_expression")
    );

    let alloc_db = temp.path().join("v2-alloc-effect.sqlite");
    run(&["init", path(&alloc_db)]);
    run(&["import", path(&alloc_db), "examples/v2/box_heap.cdb"]);
    let alloc = parse_json(&run(&["why-effect", path(&alloc_db), "main", "--json"]));
    assert!(
        alloc["declared_effects"]
            .as_array()
            .unwrap()
            .contains(&json!("alloc"))
    );
    assert!(!alloc["required_by"]["alloc"].as_array().unwrap().is_empty());
    assert_eq!(
        alloc["symbol_blame"]["birth_migration"]["operation_kind"],
        "create_function"
    );

    let io_db = temp.path().join("v2-platform-effect.sqlite");
    run(&["init", path(&io_db)]);
    run(&["import", path(&io_db), "examples/v2/static_write.cdb"]);
    let io = parse_json(&run(&["why-effect", path(&io_db), "main", "--json"]));
    assert!(
        io["declared_effects"]
            .as_array()
            .unwrap()
            .contains(&json!("io"))
    );
    assert!(!io["required_by"]["io"].as_array().unwrap().is_empty());

    let platform = parse_json(&run(&[
        "why-platform-extern",
        path(&io_db),
        "main",
        "write",
        "--json",
    ]));
    assert_eq!(platform["schema"], "codedb/why-platform-extern/v1");
    assert!(
        platform["platform_external_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|external| external["link_name"] == "write")
    );
    assert_eq!(
        platform["semantic_externals"][0]["blame"]["birth_migration"]["operation_kind"],
        "create_external_function"
    );
    assert!(
        platform["build_plan"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability["name"] == "stdout")
    );

    let platform_text = run(&["why-platform-extern", path(&io_db), "main", "write"]);
    assert!(platform_text.contains(&format!(
        "build_plan_link_plan_input_hash {}\n",
        platform["build_plan"]["link_plan_input_hash"]
            .as_str()
            .unwrap()
    )));
    assert!(platform_text.contains(&format!(
        "build_plan_link_plan_cache_key {}\n",
        platform["build_plan"]["link_plan_cache_key"].as_str().unwrap()
    )));
    assert!(platform_text.contains("platform_extern "));
    assert!(platform_text.contains(" write\n"));
    assert!(platform_text.contains("capability stdout\n"));
}
