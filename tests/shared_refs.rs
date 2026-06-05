use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
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
fn line_view_refs_compile_trace_round_trip_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("line-view-refs.sqlite");
    let projection = temp.path().join("line-view-refs.projection.cdb");
    let rebuilt = temp.path().join("line-view-refs-rebuilt.sqlite");
    let main_ir_path = temp.path().join("main.ir.json");
    let total_ir_path = temp.path().join("line-total.ir.json");
    let object_path = temp.path().join("main.o");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/v2/line_view_refs.cdb"]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");

    let trace = parse_json(&run(&["trace", path(&db), "main", "--json"]));
    assert_eq!(trace["status"], "ok");
    assert_eq!(trace["result"], json!({"kind": "i64", "value": "100"}));
    let trace_kinds = trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "eval_expr")
        .filter_map(|event| event["expr_kind"].as_str())
        .collect::<Vec<_>>();
    assert!(trace_kinds.contains(&"borrow_shared"));
    assert!(trace_kinds.contains(&"field_access"));

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains("record LineView<'a>"));
    assert!(exported.contains("line: &'a Line"));
    assert!(exported.contains("fn line_total<'a>(view: LineView<'a>) -> i64"));
    assert!(exported.contains("&'a line"));

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    assert_eq!(run(&["eval", path(&rebuilt), "main"]).trim(), "100");
    run(&["verify", path(&rebuilt)]);

    run(&["emit-ir", path(&db), "main", "--out", path(&main_ir_path)]);
    run(&[
        "emit-ir",
        path(&db),
        "line_total",
        "--out",
        path(&total_ir_path),
    ]);
    let main_ir = read_json(&main_ir_path);
    let main_ops = op_names(&main_ir);
    assert!(main_ops.contains(&"borrow_shared".to_string()));
    assert!(main_ops.contains(&"borrow_debug".to_string()));

    let total_ir = read_json(&total_ir_path);
    let total_ops = op_names(&total_ir);
    assert_eq!(
        total_ops
            .iter()
            .filter(|op| op.as_str() == "deref_shared")
            .count(),
        2
    );
    assert_eq!(
        total_ops
            .iter()
            .filter(|op| op.as_str() == "addr_of_field")
            .count(),
        4
    );
    assert_eq!(
        total_ops.iter().filter(|op| op.as_str() == "load").count(),
        4
    );
    let total_fields = total_ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|op| op["op"] == "addr_of_field")
        .map(|op| op["place"]["field"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(total_fields, vec!["line", "price_cents", "line", "qty"]);

    run(&[
        "emit-object",
        path(&db),
        "main",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    let object_bytes = std::fs::read(&object_path).unwrap();
    if codedb::DEFAULT_NATIVE_TARGET == codedb::LINUX_X86_64_TARGET {
        assert_eq!(&object_bytes[..4], b"\x7fELF");
    } else {
        assert_eq!(&object_bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    }
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "line_view_refs_native",
            "--entry",
            "main",
            "--expect-i64",
            "100",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");

        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["passed"], 1);
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "100"})
        );
    }
}

#[test]
fn returning_reference_to_local_storage_is_rejected() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("leak-local-ref.sqlite");
    let source = temp.path().join("leak-local-ref.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn leak<'a>() -> &'a Line =
  let line: Line = { price_cents: 25, qty: 4 } in
  &'a line
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_borrow"))
        .stderr(predicate::str::contains(
            "returns reference to local storage",
        ));
}

#[test]
fn borrowed_field_and_polymorphic_call_returns_use_caller_region() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("borrowed-return.sqlite");
    let source = temp.path().join("borrowed-return.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineView<'a> {
  line: &'a Line
}

fn first_line<'a>(view: LineView<'a>) -> &'a Line = view.line

fn id_ref<'a>(line: &'a Line) -> &'a Line = line

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let view: LineView<'a> = { line: &'a line } in
  let borrowed: &'a Line = first_line(view) in
  let same: &'a Line = id_ref(&'a line) in
  borrowed.price_cents + same.qty
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "29");
    run(&["verify", path(&db)]);
}

#[test]
fn verify_rejects_shared_borrow_region_outside_function_scope() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("bad-shared-borrow-region.sqlite");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/v2/line_view_refs.cdb"]);
    run(&["verify", path(&db)]);

    let conn = Connection::open(&db).unwrap();
    let (borrow_hash, payload_json): (String, String) = conn
        .query_row(
            "SELECT hash, payload_json
             FROM objects
             WHERE kind = 'Expression' AND payload_json LIKE '%borrow_shared%'
             ORDER BY hash LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut payload: JsonValue = serde_json::from_str(&payload_json).unwrap();
    let original_region = payload["region"].as_str().unwrap().to_string();
    let bogus_region: String = conn
        .query_row(
            "SELECT hash
             FROM objects
             WHERE kind = 'Type' AND hash != ?1
             ORDER BY hash LIMIT 1",
            [&original_region],
            |row| row.get(0),
        )
        .unwrap();
    payload["region"] = JsonValue::String(bogus_region);
    let canonical = canonical_json(&payload);
    conn.execute(
        "UPDATE objects
         SET payload_json = ?1, payload_size_bytes = ?2
         WHERE hash = ?3",
        (&canonical, canonical.len() as i64, borrow_hash),
    )
    .unwrap();

    bin()
        .args(["verify", path(&db)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid region reference"));
}

fn op_names(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect()
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

fn canonical_json(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => serde_json::to_string(value).expect("string serialization"),
        JsonValue::Array(values) => {
            let inner = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        JsonValue::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key serialization"),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
    }
}
