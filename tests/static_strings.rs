use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, params};
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
fn static_strings_and_bytes_lower_emit_verify_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("static-strings.sqlite");
    let source = temp.path().join("static-strings.cdb");
    let projection = temp.path().join("static-strings.export.cdb");
    let rebuilt = temp.path().join("static-strings-rebuilt.sqlite");
    let ir_path = temp.path().join("literal-total.ir.json");
    let object_path = temp.path().join("literal-total.o");
    let link_plan_path = temp.path().join("literal-total.link.json");

    std::fs::write(
        &source,
        r#"
extern fn platform_write(fd: i64, ptr: raw_ptr<u8>, len: i64) -> i64 abi[c] effects[io, ffi, unsafe] link_name "write" library "c"

fn string_len() -> i64 = len("hello")
fn bytes_len() -> i64 = len(b"hi\0!")
fn literal_total() -> i64 = len("hello") + len(b"hi\0!")
fn first_byte() -> u8 = b"hello"[1]

fn write_static() -> i64 effects[io, ffi, unsafe] =
  let s: slice<'static, u8> = "hello" in
  platform_write(1, raw_ptr(&'static s[0]), len(s))
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "string_len"]).trim(), "5");
    assert_eq!(run(&["eval", path(&db), "bytes_len"]).trim(), "4");
    assert_eq!(run(&["eval", path(&db), "literal_total"]).trim(), "9");
    assert_eq!(run(&["eval", path(&db), "first_byte"]).trim(), "101");
    run(&["verify", path(&db)]);

    let trace = parse_json(&run(&["trace", path(&db), "literal_total", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "9"}));
    assert_trace_eval_kind(&trace, "static_bytes");
    assert_trace_eval_kind(&trace, "slice_len");

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("len(\"hello\")"));
    assert!(exported.contains("len(b\"hi\\0!\")"));
    assert!(exported.contains("slice<'static, u8>"));
    assert!(exported.contains("raw_ptr(&'static s[0])"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "literal_total"]).trim(), "9");
    run(&["verify", path(&rebuilt)]);

    run(&[
        "emit-ir",
        path(&db),
        "literal_total",
        "--out",
        path(&ir_path),
    ]);
    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert_eq!(
        ops.iter()
            .filter(|op| op.as_str() == "static_data_address")
            .count(),
        2
    );
    assert!(ops.contains(&"construct_slice".to_string()));
    let debug_kinds = debug_kinds(&ir);
    assert!(debug_kinds.contains(&"static_data_address".to_string()));

    run(&[
        "emit-object",
        path(&db),
        "literal_total",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    let object = std::fs::read(&object_path).unwrap();
    assert_native_object_magic(&object);
    assert!(
        object
            .windows(b"hello".len())
            .any(|window| window == b"hello")
    );
    assert!(
        object
            .windows(b"hi\0!".len())
            .any(|window| window == b"hi\0!")
    );

    let metadata = latest_object_metadata(&db);
    let static_data = metadata["static_data"]
        .as_array()
        .expect("static data metadata");
    assert_eq!(static_data.len(), 2);
    let expected_section = match codedb::DEFAULT_NATIVE_TARGET {
        codedb::LINUX_X86_64_TARGET => ".rodata",
        codedb::APPLE_ARM64_TARGET => "__TEXT,__const",
        other => panic!("unexpected native target {other}"),
    };
    assert!(static_data.iter().all(|entry| {
        entry["section"] == expected_section
            && entry["section_offset"].is_u64()
            && entry["offset"].is_u64()
    }));
    assert!(static_data.iter().any(|entry| {
        entry["bytes_hex"] == "68656c6c6f" && entry["len"] == 5 && entry["offset"].is_u64()
    }));
    assert!(static_data.iter().any(|entry| {
        entry["bytes_hex"] == "68690021" && entry["len"] == 4 && entry["offset"].is_u64()
    }));

    run(&[
        "link-native",
        path(&db),
        "literal_total",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&link_plan_path),
    ]);
    let link_plan = read_json(&link_plan_path);
    let plan_static_data = link_plan["objects"][0]["static_data"]
        .as_array()
        .expect("link plan static data metadata");
    assert_eq!(plan_static_data, static_data);

    let created = parse_json(&run(&[
        "create-test",
        path(&db),
        "static_literal_total_native",
        "--entry",
        "literal_total",
        "--expect-i64",
        "9",
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

    if can_build_default_native_target() {
        let first_byte_exe = temp.path().join("first-byte");
        run(&[
            "build",
            path(&db),
            "first_byte",
            "--out",
            path(&first_byte_exe),
        ]);
        let first_byte_status = StdCommand::new(&first_byte_exe)
            .status()
            .expect("run first_byte");
        assert_eq!(first_byte_status.code(), Some(i32::from(b'e')));

        let exe = temp.path().join("write-static");
        run(&["build", path(&db), "write_static", "--out", path(&exe)]);
        let output = StdCommand::new(&exe).output().expect("run write_static");
        assert_eq!(output.status.code(), Some(5));
        assert_eq!(output.stdout, b"hello");
    }

    corrupt_first_static_data_len(&db);
    bin()
        .args(["verify", path(&db)])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "static data len does not match bytes_hex",
        ));
}

#[test]
fn empty_static_strings_and_bytes_emit_native_objects() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("empty-static-strings.sqlite");
    let source = temp.path().join("empty-static-strings.cdb");
    let object_path = temp.path().join("empty-total.o");

    std::fs::write(
        &source,
        r#"
fn empty_string_len() -> i64 = len("")
fn empty_bytes_len() -> i64 = len(b"")
fn empty_total() -> i64 = len("") + len(b"")
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "empty_total"]).trim(), "0");
    run(&["verify", path(&db)]);

    run(&[
        "emit-object",
        path(&db),
        "empty_total",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    assert_native_object_magic(&std::fs::read(&object_path).unwrap());

    let metadata = latest_object_metadata(&db);
    let static_data = metadata["static_data"]
        .as_array()
        .expect("static data metadata");
    assert_eq!(static_data.len(), 1);
    assert_eq!(static_data[0]["bytes_hex"], "");
    assert_eq!(static_data[0]["len"], 0);
    let expected_section = match codedb::DEFAULT_NATIVE_TARGET {
        codedb::LINUX_X86_64_TARGET => ".rodata",
        codedb::APPLE_ARM64_TARGET => "__TEXT,__const",
        other => panic!("unexpected native target {other}"),
    };
    assert_eq!(static_data[0]["section"], expected_section);
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "empty_static_total_native",
            "--entry",
            "empty_total",
            "--expect-i64",
            "0",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    }
}

#[test]
fn verify_rejects_corrupt_native_object_static_data_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("static-object-metadata.sqlite");
    let source = temp.path().join("static-object-metadata.cdb");
    let object_path = temp.path().join("literal-total.o");

    std::fs::write(
        &source,
        r#"
fn literal_total() -> i64 = len("hello") + len(b"hi")
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&[
        "emit-object",
        path(&db),
        "literal_total",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    corrupt_first_object_static_data_len(&db);
    bin()
        .args(["verify", path(&db)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("static data"));
}

#[test]
fn static_write_fixture_uses_std_platform_and_io_wrapper() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("static-write-fixture.sqlite");
    let projection = temp.path().join("static-write.export.cdb");
    let source = Path::new("examples/v2/static_write.cdb");

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
    assert!(exported.contains("module std.platform"));
    assert!(exported.contains("module std.io"));
    assert!(exported.contains("std.platform.write"));
    assert!(exported.contains("std.io.write_stdout(\"hello\")"));

    let build_plan = parse_json(&run(&["build-plan", path(&db), "main", "--json"]));
    assert_eq!(build_plan["entry_effects"], json!(["io", "ffi", "unsafe"]));
    assert!(
        build_plan["external_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["link_name"] == "write")
    );

    if can_build_default_native_target() {
        let exe = temp.path().join("static-write");
        run(&["build", path(&db), "main", "--out", path(&exe)]);
        let output = StdCommand::new(&exe).output().expect("run static_write");
        assert_eq!(output.status.code(), Some(5));
        assert_eq!(output.stdout, b"hello");
    }
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

fn debug_kinds(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["debug_map"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["lowered_op_kind"].as_str().unwrap().to_string())
        .collect()
}

fn latest_object_metadata(db: &Path) -> JsonValue {
    let conn = Connection::open(db).unwrap();
    let artifact_json: String = conn
        .query_row(
            "SELECT artifact_json
             FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let wrapper: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    wrapper["metadata"].clone()
}

fn corrupt_first_static_data_len(db: &Path) {
    let conn = Connection::open(db).unwrap();
    let (hash, payload_json): (String, String) = conn
        .query_row(
            "SELECT hash, payload_json
             FROM objects
             WHERE kind = 'StaticData'
             ORDER BY hash LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let payload: JsonValue = serde_json::from_str(&payload_json).unwrap();
    let bytes_hex = payload["bytes_hex"].as_str().unwrap();
    let corrupted =
        format!(r#"{{"bytes_hex":"{bytes_hex}","len":999,"schema":"codedb/static-data/v1"}}"#);
    conn.execute(
        "UPDATE objects
         SET payload_json = ?1, payload_size_bytes = ?2
         WHERE hash = ?3",
        params![corrupted, corrupted.len() as i64, hash],
    )
    .unwrap();
}

fn corrupt_first_object_static_data_len(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute(
        "UPDATE compile_cache
         SET artifact_json = json_set(artifact_json, '$.metadata.static_data[0].len', 999)
         WHERE artifact_kind = 'object_file'
           AND artifact_json LIKE '%static_data%'",
        [],
    )
    .unwrap();
}

fn assert_trace_eval_kind(trace: &JsonValue, kind: &str) {
    assert!(
        trace["events"].as_array().unwrap().iter().any(|event| {
            event["event"] == "eval_expr" && event["expr_kind"].as_str() == Some(kind)
        }),
        "missing trace eval kind {kind}"
    );
}

fn assert_native_object_magic(bytes: &[u8]) {
    match codedb::DEFAULT_NATIVE_TARGET {
        codedb::LINUX_X86_64_TARGET => assert_eq!(&bytes[..4], b"\x7fELF"),
        codedb::APPLE_ARM64_TARGET => assert_eq!(&bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]),
        other => panic!("unexpected default native target {other}"),
    }
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
