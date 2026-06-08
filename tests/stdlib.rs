use std::collections::BTreeSet;
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

#[test]
fn stdlib_skeleton_compiles_and_build_plan_reports_capsule_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("stdlib.sqlite");
    let projection = temp.path().join("stdlib.export.cdb");
    let link_plan_path = temp.path().join("stdlib.link.json");
    let source = Path::new("examples/v2/std_minimal.cdb");

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(source)]);
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
    assert!(exported.contains("module std.core"));
    assert!(exported.contains("module std.mem"));
    assert!(exported.contains("module std.platform"));
    assert!(exported.contains("module std.io"));
    assert!(exported.contains("module std.alloc"));
    assert!(exported.contains("std.io.write_stdout(\"hello\")"));
    assert!(exported.contains("std.alloc.alloc_raw(8, 8)"));

    let build_plan = parse_json(&run(&["build-plan", path(&db), "main", "--json"]));
    assert_eq!(build_plan["schema"], "codedb/native-build-plan/v1");
    assert_json_array_contains_all(
        &build_plan["entry_effects"],
        &["io", "alloc", "ffi", "unsafe"],
    );
    assert_external_link_names(
        &build_plan["external_symbols"],
        &["free", "malloc", "write"],
    );
    assert_platform_externs(
        &build_plan["platform_external_symbols"],
        &[
            ("std.platform.free", "free"),
            ("std.platform.malloc", "malloc"),
            ("std.platform.write", "write"),
        ],
    );
    assert_capability_names(&build_plan["capabilities"], &["alloc", "stdout"]);

    run(&[
        "link-native",
        path(&db),
        "main",
        "--out",
        path(&link_plan_path),
    ]);
    let link_plan = parse_json(&std::fs::read_to_string(&link_plan_path).unwrap());
    assert_platform_externs(
        &link_plan["platform_external_symbols"],
        &[
            ("std.platform.free", "free"),
            ("std.platform.malloc", "malloc"),
            ("std.platform.write", "write"),
        ],
    );

    if can_build_default_native_target() {
        let exe = temp.path().join("stdlib-main");
        run(&["build", path(&db), "main", "--out", path(&exe)]);
        let output = StdCommand::new(&exe).output().expect("run stdlib-main");
        assert_eq!(output.status.code(), Some(17));
        assert_eq!(output.stdout, b"hello");
    }
}

#[test]
fn build_plan_reports_compiler_generated_allocation_capsule() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-platform-plan.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/v2/box_heap.cdb"]);

    let build_plan = parse_json(&run(&["build-plan", path(&db), "boxed_total", "--json"]));
    assert_platform_externs(
        &build_plan["platform_external_symbols"],
        &[("compiler.heap_alloc", "malloc"), ("compiler.drop", "free")],
    );
    assert_eq!(build_plan["capabilities"], json!([]));
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

fn assert_external_link_names(value: &JsonValue, expected: &[&str]) {
    let actual = value
        .as_array()
        .expect("external symbols")
        .iter()
        .map(|entry| entry["link_name"].as_str().expect("link_name"))
        .collect::<BTreeSet<_>>();
    for link_name in expected {
        assert!(
            actual.contains(link_name),
            "missing external link_name {link_name:?} in {actual:?}"
        );
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
