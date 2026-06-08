use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value as JsonValue;
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
fn todo_cli_reads_writes_files_and_reports_file_capabilities() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("todo-cli.sqlite");
    let sandbox = temp.path().join("sandbox");
    let projection = temp.path().join("todo-cli.export.cdb");
    let rebuilt = temp.path().join("todo-cli-rebuilt.sqlite");
    let link_plan_path = temp.path().join("todo-cli.link.json");
    let main_ir_path = temp.path().join("todo-cli-main.ir.json");
    let label_ir_path = temp.path().join("todo-cli-label.ir.json");
    let source = Path::new("examples/v2/todo_cli.cdb");

    std::fs::create_dir(&sandbox).unwrap();
    std::fs::write(sandbox.join("codedb_todo_input.txt"), b"todo: milk\n").unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(source)]);
    run(&["verify", path(&db)]);

    let build_plan = parse_json(&run(&["build-plan", path(&db), "main", "--json"]));
    assert_eq!(build_plan["schema"], "codedb/native-build-plan/v1");
    assert_json_array_contains_all(
        &build_plan["entry_effects"],
        &["io", "alloc", "ffi", "unsafe"],
    );
    assert_capability_names(
        &build_plan["capabilities"],
        &["stdout", "read_file", "write_file"],
    );
    assert_platform_externs(
        &build_plan["platform_external_symbols"],
        &[
            ("std.platform.close", "close"),
            ("std.platform.creat", "creat"),
            ("compiler.drop", "free"),
            ("compiler.heap_alloc", "malloc"),
            ("std.platform.open", "open"),
            ("std.platform.read", "read"),
            ("std.platform.write", "write"),
        ],
    );

    run(&[
        "link-native",
        path(&db),
        "main",
        "--out",
        path(&link_plan_path),
    ]);
    let link_plan = read_json(&link_plan_path);
    assert_eq!(link_plan["entry_point"], build_plan["entry_point"]);
    assert_platform_externs(
        &link_plan["platform_external_symbols"],
        &[
            ("std.platform.close", "close"),
            ("std.platform.creat", "creat"),
            ("std.platform.open", "open"),
            ("std.platform.read", "read"),
            ("std.platform.write", "write"),
        ],
    );

    run(&["emit-ir", path(&db), "main", "--out", path(&main_ir_path)]);
    let main_ir = read_json(&main_ir_path);
    let main_ops = op_names(&main_ir);
    assert!(main_ops.contains(&"call".to_string()));

    run(&[
        "emit-ir",
        path(&db),
        "dynamic_label_len",
        "--out",
        path(&label_ir_path),
    ]);
    let label_ir = read_json(&label_ir_path);
    let label_ops = op_names(&label_ir);
    assert!(label_ops.contains(&"string_new".to_string()));
    assert!(label_ops.contains(&"drop".to_string()));

    let cli_report = parse_json(&run(&[
        "test-cli",
        path(&db),
        "main",
        "--expect-stdout",
        "todo_cli copied\n",
        "--expect-exit-code",
        "0",
        "--cwd",
        path(&sandbox),
        "--json",
    ]));
    assert_eq!(cli_report["schema"], "codedb/native-cli-test-result/v1");
    assert_eq!(cli_report["native_required"], true);
    if can_build_default_native_target() {
        assert_eq!(cli_report["status"], "passed");
        assert_eq!(cli_report["comparison"]["stdout_matches"], true);
        assert_eq!(cli_report["comparison"]["exit_code_matches"], true);
        assert_eq!(
            std::fs::read(sandbox.join("codedb_todo_output.txt")).unwrap(),
            b"todo: milk\n"
        );

        std::fs::remove_file(sandbox.join("codedb_todo_output.txt")).unwrap();
        let exe = temp.path().join("todo-cli-main");
        run(&["build", path(&db), "main", "--out", path(&exe)]);
        let output = StdCommand::new(&exe)
            .current_dir(&sandbox)
            .output()
            .expect("run todo_cli");
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, b"todo_cli copied\n");
        assert_eq!(
            std::fs::read(sandbox.join("codedb_todo_output.txt")).unwrap(),
            b"todo: milk\n"
        );
    } else {
        assert_eq!(cli_report["status"], "unsupported");
        assert_eq!(cli_report["reason_code"], "backend_unavailable");
    }

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("module std.result"));
    assert!(exported.contains("module std.io"));
    assert!(exported.contains("std.io.read_file(input_path(), data, buffer_capacity())"));
    assert!(exported.contains("std.io.write_file(output_path(), raw_ptr(data), read_len)"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    run(&["verify", path(&rebuilt)]);
}

fn op_names(ir: &JsonValue) -> Vec<String> {
    let mut out = Vec::new();
    collect_ops(&ir["ir"]["operations"], &mut out);
    out
}

fn collect_ops(ops: &JsonValue, out: &mut Vec<String>) {
    for op in ops.as_array().unwrap() {
        out.push(op["op"].as_str().unwrap().to_string());
        if let Some(then_block) = op.get("then_block") {
            collect_ops(&then_block["operations"], out);
        }
        if let Some(else_block) = op.get("else_block") {
            collect_ops(&else_block["operations"], out);
        }
        if let Some(arms) = op.get("arms").and_then(JsonValue::as_array) {
            for arm in arms {
                collect_ops(&arm["block"]["operations"], out);
            }
        }
        if let Some(body) = op.get("body") {
            collect_ops(&body["operations"], out);
        }
    }
}

fn assert_json_array_contains_all(value: &JsonValue, expected: &[&str]) {
    let actual = value
        .as_array()
        .expect("json array")
        .iter()
        .map(|value| value.as_str().expect("string value"))
        .collect::<BTreeSet<_>>();
    for item in expected {
        assert!(actual.contains(item), "missing {item:?} in {actual:?}");
    }
}

fn assert_platform_externs(value: &JsonValue, expected: &[(&str, &str)]) {
    let entries = value.as_array().expect("platform externs");
    for (source, link_name) in expected {
        assert!(
            entries.iter().any(|entry| {
                entry["source"].as_str() == Some(*source)
                    && entry["link_name"].as_str() == Some(*link_name)
                    && entry["platform"].as_bool() == Some(true)
            }),
            "missing platform extern source={source:?} link_name={link_name:?} in {entries:?}"
        );
    }
}

fn assert_capability_names(value: &JsonValue, expected: &[&str]) {
    let actual = value
        .as_array()
        .expect("capabilities")
        .iter()
        .map(|entry| entry["name"].as_str().expect("capability name"))
        .collect::<BTreeSet<_>>();
    for name in expected {
        assert!(
            actual.contains(name),
            "missing capability {name:?} in {actual:?}"
        );
    }
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
