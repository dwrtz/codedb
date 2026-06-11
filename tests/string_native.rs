// Phase 12 (R15): a real string surface (`std/string.cdb`) over the dynamic
// buffer — index, compare, concat, substring, push. The acceptance program
// concatenates, compares, and indexes strings natively, and the oracle is
// eval == native. New buffer primitives (`string_with_capacity`, `string_push`,
// `string_get`) are pinned in the lowered IR.
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

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
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

fn op_names(db: &Path, entry: &str) -> Vec<String> {
    let temp = tempdir().unwrap();
    let ir_path = temp.path().join("ir.json");
    run(&["emit-ir", path(db), entry, "--out", path(&ir_path)]);
    let ir = read_json(&ir_path);
    let mut out = Vec::new();
    collect_ops(&ir["ir"]["operations"], &mut out);
    out
}

/// (entry, expected i64). bool results are mapped to 1/0 by the driver.
const CASES: &[(&str, &str)] = &[
    ("concat_len", "7"),
    ("concat_eq", "1"),
    ("eq_true", "1"),
    ("eq_false", "0"),
    ("index_at", "97"),
    ("cmp_lt", "-1"),
    ("cmp_gt", "1"),
    ("cmp_eq", "0"),
    ("cmp_prefix", "-1"),
    ("sub_eq", "1"),
];

const DRIVER: &str = r#"
fn concat_len() -> i64 effects[alloc, state] =
  let c: string = std.string.concat(string_new("foo"), string_new("bar!")) in string_len(c)
fn concat_eq() -> i64 effects[alloc, state] =
  let c: string = std.string.concat(string_new("foo"), string_new("bar!")) in
  if std.string.eq(c, string_new("foobar!")) then 1 else 0
fn eq_true() -> i64 effects[alloc, state] =
  if std.string.eq(string_new("hello"), string_new("hello")) then 1 else 0
fn eq_false() -> i64 effects[alloc, state] =
  if std.string.eq(string_new("abc"), string_new("abd")) then 1 else 0
fn index_at() -> i64 effects[alloc, state] =
  let c: string = std.string.concat(string_new("foo"), string_new("bar")) in
  to_i64(std.string.get(c, 4))
fn cmp_lt() -> i64 effects[alloc, state] =
  std.string.compare(string_new("abc"), string_new("abd"))
fn cmp_gt() -> i64 effects[alloc, state] =
  std.string.compare(string_new("abd"), string_new("abc"))
fn cmp_eq() -> i64 effects[alloc, state] =
  std.string.compare(string_new("abc"), string_new("abc"))
fn cmp_prefix() -> i64 effects[alloc, state] =
  std.string.compare(string_new("ab"), string_new("abc"))
fn sub_eq() -> i64 effects[alloc, state] =
  let s: string = std.string.substring(string_new("hello"), 1, 4) in
  if std.string.eq(s, string_new("ell")) then 1 else 0
"#;

#[test]
fn std_string_concat_compare_index_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("string.sqlite");
    let driver = temp.path().join("string_driver.cdb");
    std::fs::write(&driver, DRIVER).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), "std/string.cdb"]);
    run(&["import", path(&db), path(&driver)]);
    run(&["verify", path(&db)]);

    // Reference-evaluator oracle.
    for (entry, expected) in CASES {
        assert_eq!(
            run(&["eval", path(&db), entry]).trim(),
            *expected,
            "eval mismatch for {entry}"
        );
    }

    // The new buffer primitives are real lowered ops, not desugared away.
    let push_range_ops = op_names(&db, "std.string.push_range");
    assert!(
        push_range_ops.iter().any(|op| op == "string_push"),
        "push_range should lower a string_push: {push_range_ops:?}"
    );
    assert!(
        push_range_ops.iter().any(|op| op == "string_get"),
        "push_range should lower a string_get: {push_range_ops:?}"
    );
    let concat_ops = op_names(&db, "std.string.concat");
    assert!(
        concat_ops.iter().any(|op| op == "string_with_capacity"),
        "concat should lower a string_with_capacity: {concat_ops:?}"
    );

    for (entry, expected) in CASES {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            &format!("{entry}_native"),
            "--entry",
            entry,
            &format!("--expect-i64={expected}"),
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied", "create-test {entry}");
    }

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed", "native string report: {report}");
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["unsupported"], 0);
    } else {
        assert!(report["tests"].as_array().is_some());
    }
}

// The headline acceptance program (SPEC_V3 §12 / PLAN_V3 Phase 12): one native
// binary that concatenates, compares, and indexes strings AND round-trips an i64
// through formatting. `acceptance` returns 1 only if every check holds, so a
// single native i64 result gates the whole sentence.
const ACCEPTANCE: &str = r#"
fn acceptance() -> i64 effects[alloc, state] =
  let greeting: string = std.string.concat(string_new("foo"), string_new("bar")) in
  let is_foobar: bool = std.string.eq(greeting, string_new("foobar")) in
  let third: u8 = std.string.get(std.string.concat(string_new("foo"), string_new("bar")), 3) in
  let ordered: bool = std.string.compare(string_new("apple"), string_new("banana")) < 0 in
  let n: i64 = std.fmt.string_to_i64(std.fmt.i64_to_string(0 - 1234567)) in
  if is_foobar then
    if third == b"b"[0] then
      if ordered then
        if n == 0 - 1234567 then 1 else 0
      else 0
    else 0
  else 0
"#;

#[test]
fn acceptance_program_concatenates_compares_indexes_and_formats_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("acceptance.sqlite");
    let driver = temp.path().join("acceptance.cdb");
    std::fs::write(&driver, ACCEPTANCE).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), "std/string.cdb"]);
    run(&["import", path(&db), "std/fmt.cdb"]);
    run(&["import", path(&db), path(&driver)]);
    run(&["verify", path(&db)]);

    assert_eq!(run(&["eval", path(&db), "acceptance"]).trim(), "1");

    let created = parse_json(&run(&[
        "create-test",
        path(&db),
        "acceptance_native",
        "--entry",
        "acceptance",
        "--expect-i64",
        "1",
        "--native-required",
        "--json",
    ]));
    assert_eq!(created["status"], "applied");

    let report = parse_json(&run(&["test", path(&db), "--json"]));
    if can_build_default_native_target() {
        assert_eq!(report["status"], "passed", "acceptance report: {report}");
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
    } else {
        assert_eq!(report["tests"][0]["native"]["status"], "unsupported");
    }
}
