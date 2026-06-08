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
fn dynamic_vec_and_string_lower_verify_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("dynamic-buffers.sqlite");
    let source = temp.path().join("dynamic-buffers.cdb");
    let vec_ir_path = temp.path().join("vec-total.ir.json");
    let string_ir_path = temp.path().join("string-len.ir.json");
    let vec_layout_path = temp.path().join("vec.layout.json");
    let string_layout_path = temp.path().join("string.layout.json");
    let object_path = temp.path().join("vec-total.o");

    std::fs::write(
        &source,
        r#"
fn vec_total() -> i64 effects[alloc, state] =
  let xs: vec<i64> = vec_new(3) in
  let pushed1: unit = vec_push(xs, 10) in
  let pushed2: unit = vec_push(xs, 32) in
  vec_get(xs, 0) + vec_get(xs, 1) + vec_len(xs)

fn owned_string_len() -> i64 effects[alloc] =
  let s: string = string_new("hello") in
  string_len(s)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "vec_total"]).trim(), "44");
    assert_eq!(run(&["eval", path(&db), "owned_string_len"]).trim(), "5");
    run(&["verify", path(&db)]);

    run(&[
        "emit-type-layout",
        path(&db),
        "vec<i64>",
        "--out",
        path(&vec_layout_path),
    ]);
    let vec_layout = read_json(&vec_layout_path);
    assert_eq!(vec_layout["kind"], "vec");
    assert_eq!(vec_layout["copy_kind"], "move_only");
    assert_eq!(vec_layout["drop_kind"], "needs_drop");
    assert_eq!(vec_layout["abi"]["pass"], "by_indirect");

    run(&[
        "emit-type-layout",
        path(&db),
        "string",
        "--out",
        path(&string_layout_path),
    ]);
    let string_layout = read_json(&string_layout_path);
    assert_eq!(string_layout["kind"], "string");
    assert_eq!(string_layout["copy_kind"], "move_only");
    assert_eq!(string_layout["drop_kind"], "needs_drop");
    assert_eq!(string_layout["abi"]["pass"], "by_indirect");

    run(&[
        "emit-ir",
        path(&db),
        "vec_total",
        "--out",
        path(&vec_ir_path),
    ]);
    let vec_ir = read_json(&vec_ir_path);
    let vec_ops = op_names(&vec_ir);
    assert!(vec_ops.contains(&"vec_new".to_string()));
    assert!(vec_ops.contains(&"vec_push".to_string()));
    assert!(vec_ops.contains(&"vec_get".to_string()));
    assert!(vec_ops.contains(&"vec_len".to_string()));
    assert!(vec_ops.contains(&"drop".to_string()));

    run(&[
        "emit-ir",
        path(&db),
        "owned_string_len",
        "--out",
        path(&string_ir_path),
    ]);
    let string_ir = read_json(&string_ir_path);
    let string_ops = op_names(&string_ir);
    assert!(string_ops.contains(&"string_new".to_string()));
    assert!(string_ops.contains(&"string_len".to_string()));
    assert!(string_ops.contains(&"drop".to_string()));

    run(&[
        "emit-object",
        path(&db),
        "vec_total",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    assert_native_object_magic(&std::fs::read(&object_path).unwrap());

    if can_build_default_native_target() {
        let vec_test = parse_json(&run(&[
            "create-test",
            path(&db),
            "dynamic_vec_native",
            "--entry",
            "vec_total",
            "--expect-i64",
            "44",
            "--native-required",
            "--json",
        ]));
        assert_eq!(vec_test["status"], "applied");
        let string_test = parse_json(&run(&[
            "create-test",
            path(&db),
            "dynamic_string_native",
            "--entry",
            "owned_string_len",
            "--expect-i64",
            "5",
            "--native-required",
            "--json",
        ]));
        assert_eq!(string_test["status"], "applied");

        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        let mut actuals = report["tests"]
            .as_array()
            .expect("test reports")
            .iter()
            .map(|test| test["native"]["comparison"]["actual"].clone())
            .collect::<Vec<_>>();
        actuals.sort_by_key(|value| value["value"].as_str().unwrap_or_default().to_string());
        assert_eq!(
            actuals,
            vec![
                json!({"kind": "i64", "value": "44"}),
                json!({"kind": "i64", "value": "5"}),
            ]
        );
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
