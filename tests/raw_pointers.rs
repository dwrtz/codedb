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

fn run_fail(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
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
fn raw_pointer_load_store_trace_export_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("raw-pointers.sqlite");
    let source = temp.path().join("raw-pointers.cdb");
    let projection = temp.path().join("raw-pointers.export.cdb");
    let rebuilt = temp.path().join("raw-pointers-rebuilt.sqlite");
    let ir_path = temp.path().join("store-mut.ir.json");

    std::fs::write(
        &source,
        r#"
fn load_shared<'a>() -> i64 effects[unsafe] =
  let x: i64 = 41 in
  raw_load(raw_ptr(&'a x))

fn store_mut<'a>() -> i64 effects[state, unsafe] =
  let x: i64 = 1 in
  let p: raw_mut_ptr<i64> = raw_mut_ptr(&'a mut x) in
  let ignored: unit = raw_store(p, 9) in
  x

fn cast_mut_to_const<'a>() -> i64 effects[state, unsafe] =
  let x: i64 = 6 in
  let p: raw_mut_ptr<i64> = raw_mut_ptr(&'a mut x) in
  raw_load(raw_ptr(p))

fn main<'a>() -> i64 effects[state, unsafe] =
  load_shared() + store_mut() + cast_mut_to_const()
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "56");
    run(&["verify", path(&db)]);

    run(&["emit-ir", path(&db), "store_mut", "--out", path(&ir_path)]);
    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert!(ops.contains(&"ptr_cast".to_string()));
    assert!(ops.contains(&"deref_raw".to_string()));
    assert!(ops.contains(&"store".to_string()));
    assert!(
        ir["ir"]["debug_map"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["lowered_op_kind"] == "deref_raw")
    );

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "56"}));
    assert_trace_eval_kind(&trace, "raw_ptr_cast");
    assert_trace_eval_kind(&trace, "raw_load");
    assert_trace_eval_kind(&trace, "raw_store");

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("raw_ptr(&'a x)"));
    assert!(exported.contains("raw_mut_ptr<i64>"));
    assert!(exported.contains("effects[state, unsafe]"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "56");
    run(&["verify", path(&rebuilt)]);

    let created = parse_json(&run(&[
        "create-test",
        path(&db),
        "raw_pointer_main_native",
        "--entry",
        "main",
        "--expect-i64",
        "56",
        "--native-required",
        "--json",
    ]));
    assert_eq!(created["status"], "applied");
    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    } else {
        assert_eq!(report["status"], "failed");
        assert_eq!(report["tests"][0]["native"]["status"], "unsupported");
    }
}

#[test]
fn raw_pointer_return_values_compile_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("raw-pointer-return.sqlite");
    let source = temp.path().join("raw-pointer-return.cdb");
    let ptr_object = temp.path().join("ptr.o");

    std::fs::write(
        &source,
        r#"
fn ptr<'a>(x: &'a i64) -> raw_ptr<i64> effects[unsafe] =
  raw_ptr(x)

fn main<'a>() -> i64 effects[unsafe] =
  let x: i64 = 13 in
  raw_load(ptr(&'a x))
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "13");

    run(&[
        "emit-object",
        path(&db),
        "ptr",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&ptr_object),
    ]);
    assert_native_object_magic(&std::fs::read(&ptr_object).unwrap());

    let created = parse_json(&run(&[
        "create-test",
        path(&db),
        "raw_pointer_return_native",
        "--entry",
        "main",
        "--expect-i64",
        "13",
        "--native-required",
        "--json",
    ]));
    assert_eq!(created["status"], "applied");
    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    } else {
        assert_eq!(report["status"], "failed");
        assert_eq!(report["tests"][0]["native"]["status"], "unsupported");
    }
}

#[test]
fn raw_pointer_operations_require_unsafe_and_store_requires_state() {
    let temp = tempdir().unwrap();

    let missing_unsafe_db = temp.path().join("raw-missing-unsafe.sqlite");
    let missing_unsafe_source = temp.path().join("raw-missing-unsafe.cdb");
    std::fs::write(
        &missing_unsafe_source,
        r#"
fn bad<'a>() -> i64 =
  let x: i64 = 1 in
  raw_load(raw_ptr(&'a x))
"#,
    )
    .unwrap();
    run(&["init", path(&missing_unsafe_db)]);
    let stderr = run_fail(&[
        "import",
        path(&missing_unsafe_db),
        path(&missing_unsafe_source),
    ]);
    assert!(stderr.contains("unsafe"), "{stderr}");

    let missing_state_db = temp.path().join("raw-missing-state.sqlite");
    let missing_state_source = temp.path().join("raw-missing-state.cdb");
    std::fs::write(
        &missing_state_source,
        r#"
fn bad<'a>() -> i64 effects[unsafe] =
  let x: i64 = 1 in
  let p: raw_mut_ptr<i64> = raw_mut_ptr(&'a mut x) in
  let ignored: unit = raw_store(p, 2) in
  x
"#,
    )
    .unwrap();
    run(&["init", path(&missing_state_db)]);
    let stderr = run_fail(&[
        "import",
        path(&missing_state_db),
        path(&missing_state_source),
    ]);
    assert!(stderr.contains("state"), "{stderr}");
}

#[test]
fn raw_pointer_ffi_declarations_validate_unsafe_effect() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("raw-ffi.sqlite");
    let good = temp.path().join("raw-ffi.cdb");
    let bad_db = temp.path().join("raw-ffi-bad.sqlite");
    let bad = temp.path().join("raw-ffi-bad.cdb");
    let bad_named_db = temp.path().join("raw-ffi-bad-named.sqlite");
    let bad_named = temp.path().join("raw-ffi-bad-named.cdb");
    let link_plan = temp.path().join("raw-ffi-link.json");

    std::fs::write(
        &good,
        r#"
extern fn platform_write(fd: i64, ptr: raw_ptr<i64>, len: i64) -> i64 abi[c] effects[io, ffi, unsafe] link_name "write" library "c"
extern fn platform_malloc(size: i64, align: i64) -> raw_mut_ptr<i64> abi[c] effects[alloc, ffi, unsafe] link_name "malloc" library "c"
extern fn platform_free(ptr: raw_mut_ptr<i64>) -> unit abi[c] effects[alloc, ffi, unsafe] link_name "free" library "c"
fn main<'a>() -> i64 effects[io, ffi, unsafe] =
  let x: i64 = 0 in
  platform_write(1, raw_ptr(&'a x), 0)
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&good)]);
    run(&["verify", path(&db)]);
    let listed = parse_json(&run(&["list", path(&db), "--json"]));
    let write = listed["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "platform_write")
        .unwrap();
    assert_eq!(write["effects"], json!(["io", "ffi", "unsafe"]));
    assert_eq!(write["external"]["link_name"], "write");

    run(&[
        "link-native",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&link_plan),
    ]);
    let plan = read_json(&link_plan);
    assert_eq!(plan["external_symbols"].as_array().unwrap().len(), 1);
    assert_eq!(plan["external_symbols"][0]["link_name"], "write");

    std::fs::write(
        &bad,
        r#"
extern fn platform_write(fd: i64, ptr: raw_ptr<i64>, len: i64) -> i64 abi[c] effects[io, ffi] link_name "write" library "c"
"#,
    )
    .unwrap();
    run(&["init", path(&bad_db)]);
    let stderr = run_fail(&["import", path(&bad_db), path(&bad)]);
    assert!(stderr.contains("unsafe"), "{stderr}");

    std::fs::write(
        &bad_named,
        r#"
record RawSlot {
  p: raw_ptr<i64>
}

extern fn host(slot: RawSlot) -> i64 abi[c] effects[ffi] link_name "host" library "c"
"#,
    )
    .unwrap();
    run(&["init", path(&bad_named_db)]);
    let stderr = run_fail(&["import", path(&bad_named_db), path(&bad_named)]);
    assert!(stderr.contains("unsafe"), "{stderr}");
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

fn assert_trace_eval_kind(trace: &JsonValue, kind: &str) {
    assert!(
        trace["events"].as_array().unwrap().iter().any(|event| {
            event["event"] == "eval_expr" && event["expr_kind"].as_str() == Some(kind)
        }),
        "missing trace eval kind {kind}"
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
