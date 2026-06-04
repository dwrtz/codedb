use std::path::Path;

use assert_cmd::Command;
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

fn run_failure(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

#[test]
fn extern_declarations_round_trip_and_link_plan_lists_externals() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("ffi.sqlite");
    let source = temp.path().join("ffi.cdb");
    let projection = temp.path().join("ffi.projection.cdb");
    let main_object = temp.path().join("main.o");
    let link_plan = temp.path().join("link-plan.json");

    std::fs::write(
        &source,
        r#"
extern fn host_value() -> i64 abi[c] effects[io, ffi] link_name "host_value" library "c"
fn main() -> i64 effects[io, ffi] = host_value() + 1
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    let listed = parse_json(&run(&["list", path(&db), "--json"]));
    let host = listed["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "host_value")
        .unwrap();
    assert_eq!(host["definition_kind"], "external_function");
    assert_eq!(host["effects"], json!(["io", "ffi"]));
    assert_eq!(host["external"]["abi"], "c");
    assert_eq!(host["external"]["link_name"], "host_value");
    assert_eq!(host["external"]["library"], "c");

    let show_host = parse_json(&run(&["show", path(&db), "host_value", "--json"]));
    assert_eq!(show_host["definition_kind"], "external_function");
    assert_eq!(show_host["body_hash"], JsonValue::Null);
    assert_eq!(show_host["external"]["link_name"], "host_value");

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(exported.contains(
        "extern fn host_value() -> i64 abi[c] effects[io, ffi] link_name \"host_value\" library \"c\""
    ));
    assert!(exported.contains("fn main() -> i64 effects[io, ffi] = host_value() + 1"));

    run(&[
        "emit-object",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&main_object),
    ]);
    let object = std::fs::read(&main_object).unwrap();
    assert!(
        object
            .windows(b"host_value".len())
            .any(|window| window == b"host_value")
    );

    run(&[
        "link-native",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&link_plan),
    ]);
    let plan = parse_json(&std::fs::read_to_string(&link_plan).unwrap());
    assert_eq!(plan["objects"].as_array().unwrap().len(), 1);
    assert_eq!(plan["external_symbols"].as_array().unwrap().len(), 1);
    assert_eq!(plan["external_symbols"][0]["link_name"], "host_value");
    assert_eq!(plan["external_symbols"][0]["library"], "c");

    run(&["verify", path(&db)]);
}

#[test]
fn pure_function_cannot_silently_call_external_io_ffi() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("ffi-effects.sqlite");
    let apply = temp.path().join("ffi-effects.apply.json");

    std::fs::write(
        &apply,
        serde_json::to_string_pretty(&json!({
            "schema": "codedb/apply/v1",
            "operations": [
                {
                    "kind": "create_external_function",
                    "name": "host_value",
                    "birth_seed": "ffi-host-value",
                    "params": [],
                    "return_type": "i64",
                    "effects": ["io", "ffi"],
                    "abi": "c",
                    "link_name": "host_value",
                    "library": "c"
                },
                {
                    "kind": "create_function",
                    "name": "main",
                    "birth_seed": "ffi-main",
                    "params": [],
                    "return_type": "i64",
                    "body": {
                        "kind": "call",
                        "name": "host_value",
                        "args": []
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    run(&["init", path(&db)]);
    let result = parse_json(&run(&["apply", path(&db), "--json", path(&apply)]));
    assert_eq!(result["status"], "error");
    assert_eq!(result["committed"], false);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("undeclared effect io")
    );
    let listed = parse_json(&run(&["list", path(&db), "--json"]));
    assert!(listed["symbols"].as_array().unwrap().is_empty());
}

#[test]
fn verify_reports_missing_external_link_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("ffi-corrupt.sqlite");
    let source = temp.path().join("ffi-corrupt.cdb");

    std::fs::write(
        &source,
        r#"extern fn host_value() -> i64 abi[c] effects[io, ffi] link_name "host_value""#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    let show_host = parse_json(&run(&["show", path(&db), "host_value", "--json"]));
    let definition = show_host["definition_hash"].as_str().unwrap();

    let conn = Connection::open(&db).unwrap();
    let payload_json: String = conn
        .query_row(
            "SELECT payload_json FROM objects WHERE hash = ?1",
            params![definition],
            |row| row.get(0),
        )
        .unwrap();
    let mut payload: JsonValue = serde_json::from_str(&payload_json).unwrap();
    payload.as_object_mut().unwrap().remove("link_name");
    conn.execute(
        "UPDATE objects SET payload_json = ?1 WHERE hash = ?2",
        params![serde_json::to_string(&payload).unwrap(), definition],
    )
    .unwrap();

    let stderr = run_failure(&["verify", path(&db)]);
    assert!(stderr.contains("missing link_name"), "{stderr}");
}
