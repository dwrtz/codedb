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

fn read_json(path: &Path) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn word_count_fixture_processes_static_text_natively_and_round_trips() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("word-count.sqlite");
    let rebuilt = temp.path().join("word-count-rebuilt.sqlite");
    let projection = temp.path().join("word-count.export.cdb");
    let count_ir_path = temp.path().join("count-words.ir.json");
    let space_ir_path = temp.path().join("is-ascii-space.ir.json");
    let trap_ir_path = temp.path().join("bounds-trap.ir.json");
    let object_path = temp.path().join("count-words.o");
    let source = Path::new("examples/v2/word_count.cdb");

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "4");
    assert_eq!(run(&["eval", path(&db), "count_fixture"]).trim(), "4");
    run(&["verify", path(&db)]);

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "4"}));
    assert_eq!(
        trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == "loop_iteration")
            .count(),
        19
    );
    assert_trace_eval_kind(&trace, "fold");
    assert_trace_eval_kind(&trace, "static_bytes");
    assert_trace_eval_kind(&trace, "array_index");
    assert_trace_eval_kind(&trace, "binary");

    run(&[
        "emit-ir",
        path(&db),
        "count_words",
        "--out",
        path(&count_ir_path),
    ]);
    let count_ir = read_json(&count_ir_path);
    let count_ops = op_names(&count_ir);
    assert!(count_ops.contains(&"fold".to_string()));
    assert!(count_ops.contains(&"slice_len".to_string()));
    assert!(debug_kinds(&count_ir).contains(&"fold".to_string()));

    run(&[
        "emit-ir",
        path(&db),
        "std.string.is_ascii_space",
        "--out",
        path(&space_ir_path),
    ]);
    let space_ir = read_json(&space_ir_path);
    assert!(
        nested_binary_kinds(&space_ir)
            .iter()
            .any(|kind| kind == "eq_u8")
    );

    run(&[
        "emit-ir",
        path(&db),
        "bounds_trap",
        "--out",
        path(&trap_ir_path),
    ]);
    let trap_ir = read_json(&trap_ir_path);
    assert!(op_names(&trap_ir).contains(&"bounds_check".to_string()));
    assert!(debug_kinds(&trap_ir).contains(&"bounds_check".to_string()));

    let trap_trace = parse_json(&run(&["trace", path(&db), "bounds_trap", "--json"]));
    assert_eq!(trap_trace["status"], "error");
    assert_eq!(trap_trace["diagnostics"][0]["kind"], "trap");
    let trap_expr_hash = trap_trace["diagnostics"][0]["location"]["expr_hash"]
        .as_str()
        .expect("trap expr hash");
    assert!(
        trap_ir["ir"]["debug_map"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["lowered_op_kind"] == "bounds_check"
                && op["expr_hash"].as_str() == Some(trap_expr_hash))
    );

    run(&[
        "emit-object",
        path(&db),
        "count_words",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    assert_native_object_magic(&std::fs::read(&object_path).unwrap());

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("module std.string"));
    assert!(exported.contains("fn is_ascii_space(ch: u8) -> bool"));
    assert!(exported.contains("fold ch in bytes with state"));
    assert!(exported.contains("std.string.is_ascii_space(ch)"));
    assert!(exported.contains("fn bounds_trap() -> u8"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "4");
    run(&["verify", path(&rebuilt)]);

    let created = parse_json(&run(&[
        "create-test",
        path(&db),
        "word_count_native",
        "--entry",
        "main",
        "--expect-i64",
        "4",
        "--native-required",
        "--json",
    ]));
    assert_eq!(created["status"], "applied");
    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "4"})
        );

        let exe = temp.path().join("word-count-main");
        run(&["build", path(&db), "main", "--out", path(&exe)]);
        let status = StdCommand::new(&exe).status().expect("run word count");
        assert_eq!(status.code(), Some(4));

        let trap_exe = temp.path().join("word-count-bounds-trap");
        run(&["build", path(&db), "bounds_trap", "--out", path(&trap_exe)]);
        let trap_status = StdCommand::new(&trap_exe)
            .status()
            .expect("run word count bounds trap");
        assert!(!trap_status.success());
    } else {
        assert_eq!(report["status"], "failed");
        assert_eq!(report["tests"][0]["native"]["status"], "unsupported");
    }
}

fn op_names(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect()
}

fn debug_kinds(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["debug_map"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["lowered_op_kind"].as_str().unwrap().to_string())
        .collect()
}

fn nested_binary_kinds(ir: &JsonValue) -> Vec<String> {
    let mut kinds = Vec::new();
    collect_binary_kinds(&ir["ir"]["operations"], &mut kinds);
    kinds
}

fn collect_binary_kinds(value: &JsonValue, kinds: &mut Vec<String>) {
    match value {
        JsonValue::Array(values) => {
            for value in values {
                collect_binary_kinds(value, kinds);
            }
        }
        JsonValue::Object(object) => {
            if object.get("op").and_then(JsonValue::as_str) == Some("binary")
                && let Some(kind) = object.get("kind").and_then(JsonValue::as_str)
            {
                kinds.push(kind.to_string());
            }
            for value in object.values() {
                collect_binary_kinds(value, kinds);
            }
        }
        _ => {}
    }
}

fn assert_trace_eval_kind(trace: &JsonValue, expected: &str) {
    assert!(
        trace["events"].as_array().unwrap().iter().any(|event| {
            event["event"] == "eval_expr" && event["expr_kind"].as_str() == Some(expected)
        }),
        "missing trace eval kind {expected}"
    );
}

fn assert_native_object_magic(object_bytes: &[u8]) {
    if codedb::DEFAULT_NATIVE_TARGET == codedb::LINUX_X86_64_TARGET {
        assert_eq!(&object_bytes[..4], b"\x7fELF");
    } else {
        assert_eq!(&object_bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    }
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
