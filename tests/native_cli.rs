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
fn hello_invoice_cli_captures_stdout_exit_and_entry_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("hello-invoice.sqlite");
    let projection = temp.path().join("hello-invoice.export.cdb");
    let rebuilt = temp.path().join("hello-invoice-rebuilt.sqlite");
    let link_plan_path = temp.path().join("hello-invoice.link.json");
    let source = Path::new("examples/v2/hello_invoice.cdb");

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(source)]);
    assert_eq!(run(&["eval", path(&db), "invoice_total"]).trim(), "2900");
    run(&["verify", path(&db)]);

    let build_plan = parse_json(&run(&["build-plan", path(&db), "main", "--json"]));
    assert_eq!(build_plan["schema"], "codedb/native-build-plan/v1");
    assert_entry_point_metadata(&build_plan["entry_point"]);
    assert_capability_names(&build_plan["capabilities"], &["stdout"]);
    assert_platform_externs(
        &build_plan["platform_external_symbols"],
        &[("std.platform.write", "write")],
    );

    run(&[
        "link-native",
        path(&db),
        "main",
        "--out",
        path(&link_plan_path),
    ]);
    let link_plan = read_json(&link_plan_path);
    assert_eq!(link_plan["schema"], "codedb/link-plan/v1");
    assert_eq!(link_plan["entry_point"], build_plan["entry_point"]);
    assert_entry_point_metadata(&link_plan["entry_point"]);

    let cli_report = parse_json(&run(&[
        "test-cli",
        path(&db),
        "main",
        "--expect-stdout",
        "hello invoice total 2900\n",
        "--expect-exit-code",
        "0",
        "--json",
    ]));
    assert_eq!(cli_report["schema"], "codedb/native-cli-test-result/v1");
    assert_eq!(cli_report["native_required"], true);
    if can_build_default_native_target() {
        assert_eq!(cli_report["status"], "passed");
        assert_eq!(cli_report["actual"]["stdout"], "hello invoice total 2900\n");
        assert_eq!(cli_report["actual"]["exit_code"], 0);
        assert_eq!(cli_report["comparison"]["stdout_matches"], true);
        assert_eq!(cli_report["comparison"]["exit_code_matches"], true);
        assert_entry_point_metadata(&cli_report["entry_point"]);
    } else {
        assert_eq!(cli_report["status"], "unsupported");
        assert_eq!(cli_report["reason_code"], "backend_unavailable");
    }
    run(&["verify", path(&db)]);

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("module std.io"));
    assert!(exported.contains("std.io.write_stdout(\"hello invoice total 2900\\n\")"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(
        run(&["eval", path(&rebuilt), "invoice_total"]).trim(),
        "2900"
    );
    run(&["verify", path(&rebuilt)]);
}

fn assert_entry_point_metadata(entry_point: &JsonValue) {
    assert_eq!(entry_point["schema"], "codedb/entry-point/v1");
    assert_eq!(entry_point["kind"], "process");
    assert_eq!(entry_point["args"]["supported"], true);
    assert_eq!(entry_point["args"]["source"], "process-argv");
    assert_eq!(entry_point["stdout"]["supported"], true);
    assert_eq!(entry_point["exit_code"]["source"], "entry_return_value");
    assert_eq!(entry_point["runtime"]["semantic_interpreter"], false);
    assert_eq!(entry_point["runtime"]["dispatcher"], false);
    assert_eq!(entry_point["signature"]["param_type_hashes"], json!([]));
    assert!(
        entry_point["signature"]["return_type_hash"]
            .as_str()
            .is_some()
    );
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
        .collect::<std::collections::BTreeSet<_>>();
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
