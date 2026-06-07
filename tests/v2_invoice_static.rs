use std::path::Path;
use std::process::Command as StdCommand;

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

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn branch_state(db: &Path) -> (String, Option<String>) {
    let branches = parse_json(&run(&["branches", path(db), "--json"]));
    let branch = branches["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|branch| branch["name"] == "main")
        .expect("main branch");
    (
        branch["root_hash"].as_str().unwrap().to_string(),
        branch["history_hash"].as_str().map(str::to_string),
    )
}

#[test]
fn invoice_static_acceptance_covers_native_trace_build_verify_and_replay() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("invoice-static.sqlite");
    let projection_db = temp.path().join("invoice-static-projection.sqlite");
    let history_db = temp.path().join("invoice-static-history.sqlite");
    let projection = temp.path().join("invoice-static.projection.cdb");
    let history = temp.path().join("invoice-static.history.ndjson");
    let native_exe = temp.path().join("invoice-static-native");
    let source = Path::new("examples/v2/invoice_static.cdb");

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "145");

    let created = parse_json(&run(&[
        "create-test",
        path(&db),
        "invoice_static_total_native",
        "--entry",
        "main",
        "--expect-i64",
        "145",
        "--native-required",
        "--json",
    ]));
    assert_eq!(created["status"], "applied");

    let listed = parse_json(&run(&["test", path(&db), "--list", "--json"]));
    assert_eq!(listed["tests"][0]["mode"], "reference_and_native");
    assert_eq!(listed["tests"][0]["native_required"], true);
    assert_eq!(listed["tests"][0]["labels"], json!(["v2_native_required"]));

    bin()
        .args(["verify", path(&db)])
        .assert()
        .success()
        .stdout("verify ok\n");

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "145"}));
    assert_trace_eval_kind(&trace, "record_literal");
    assert_trace_eval_kind(&trace, "array_literal");
    assert_trace_eval_kind(&trace, "slice_from_array");
    assert_trace_eval_kind(&trace, "borrow_shared");
    assert_trace_eval_kind(&trace, "enum_construct");
    assert_trace_eval_kind(&trace, "case");
    assert_trace_eval_kind(&trace, "fold");
    assert_trace_event(&trace, "case_decision");
    assert_trace_field(&trace, "lines");
    assert_trace_field(&trace, "quantity");
    assert_trace_field(&trace, "cents");
    assert_eq!(trace_event_count(&trace, "borrow_shared"), 1);
    assert_eq!(trace_event_count(&trace, "loop_iteration"), 3);

    let build_plan = parse_json(&run(&["build-plan", path(&db), "main", "--json"]));
    assert_eq!(build_plan["schema"], "codedb/native-build-plan/v1");
    assert_eq!(
        build_plan["artifact_kinds"],
        json!(["object_file", "link_plan", "executable"])
    );
    assert_eq!(build_plan["entry_effects"], json!(["pure"]));
    assert_eq!(build_plan["external_symbols"], json!([]));
    assert_eq!(build_plan["objects"].as_array().unwrap().len(), 6);
    assert_eq!(
        build_plan["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|job| job["artifact_kind"] == "object_file")
            .count(),
        6
    );
    assert_eq!(
        build_plan["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|job| job["artifact_kind"] == "link_plan")
            .count(),
        1
    );

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("record Money"));
    assert!(exported.contains("record Invoice<'a>"));
    assert!(exported.contains("first: &'a Line"));
    assert!(exported.contains("fold line in invoice.lines with total = 0 do"));
    assert!(exported.contains("Discount::Fixed(money(first_line_total(invoice) / 12))"));
    run(&["init", path(&projection_db)]);
    run(&["import", path(&projection_db), path(&projection)]);
    assert_eq!(run(&["eval", path(&projection_db), "main"]).trim(), "145");
    run(&["verify", path(&projection_db)]);

    let source_branch = branch_state(&db);
    let replay = run(&["replay", path(&db), "--from-genesis"]);
    assert!(replay.contains("replay ok"));
    assert!(replay.contains(&format!("root {}", source_branch.0)));
    assert!(replay.contains(&format!(
        "history {}",
        source_branch.1.as_deref().unwrap_or("none")
    )));

    run(&[
        "export-history",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&history),
    ]);
    run(&["init", path(&history_db)]);
    run(&["import-history", path(&history_db), path(&history)]);
    assert_eq!(branch_state(&history_db), source_branch);
    assert_eq!(run(&["eval", path(&history_db), "main"]).trim(), "145");
    run(&["verify", path(&history_db)]);
    let replayed_listed = parse_json(&run(&["test", path(&history_db), "--list", "--json"]));
    assert_eq!(replayed_listed, listed);

    if can_build_default_native_target() {
        run(&["build", path(&db), "main", "--out", path(&native_exe)]);
        let status = StdCommand::new(&native_exe)
            .status()
            .expect("run native invoice executable");
        assert_eq!(status.code(), Some(145));

        let report = parse_json(&run(&[
            "test",
            path(&db),
            "--label",
            "v2_native_required",
            "--json",
        ]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "145"})
        );
    }
}

fn assert_trace_eval_kind(trace: &JsonValue, kind: &str) {
    assert!(
        trace["events"].as_array().unwrap().iter().any(|event| {
            event["event"] == "eval_expr" && event["expr_kind"].as_str() == Some(kind)
        }),
        "missing eval kind {kind}"
    );
}

fn assert_trace_field(trace: &JsonValue, field: &str) {
    assert!(
        trace["events"].as_array().unwrap().iter().any(|event| {
            event["event"] == "field_access" && event["field"].as_str() == Some(field)
        }),
        "missing field trace {field}"
    );
}

fn assert_trace_event(trace: &JsonValue, event_name: &str) {
    assert!(
        trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event"].as_str() == Some(event_name)),
        "missing trace event {event_name}"
    );
}

fn trace_event_count(trace: &JsonValue, event_name: &str) -> usize {
    trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"].as_str() == Some(event_name))
        .count()
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
