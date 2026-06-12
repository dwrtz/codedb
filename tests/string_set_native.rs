// Phase 8 substrate: `string_set(s, i, b)` — random-access write of byte `i` of a
// string place (bounds-checked against `len`), the write twin of `string_get` with
// `string_push`'s mutable-place discipline. It is the primitive that makes a `string`
// usable as mutable byte memory (the CodeDB-hosted evaluator's simulated frames/heap):
// `string_push` extends, `string_set` overwrites, `string_get` reads back. `len` and
// `capacity` are unchanged by a set. Oracle: eval == native; the `string_set(..)` call
// round-trips through the `.cdb` projection; malformed forms fail closed at import.
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

fn read_json(path: &Path) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn root_hash(db: &Path) -> String {
    parse_json(&run(&["history", path(db), "--json"]))["root_hash"]
        .as_str()
        .unwrap()
        .to_string()
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
        if let Some(cond) = op.get("cond").filter(|cond| cond.is_object()) {
            collect_ops(&cond["operations"], out);
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

/// (entry, expected i64).
const CASES: &[(&str, &str)] = &[
    // push 10,20,30; overwrite index 1 with 99; composite read-back.
    ("overwrite_middle", "109930"),
    // a set never changes len: len stays 3 after overwriting every index.
    ("len_unchanged", "3"),
    // last write wins on repeated sets of one index.
    ("last_write_wins", "7"),
    // first and last index are both writable; middle byte untouched.
    ("set_first_last", "52005"),
    // recursive fill-by-index over a zero-pushed buffer (the simulated-memory
    // shape the Phase 8 interpreter uses), then a recursive checksum read.
    ("fill_then_sum", "196"),
];

const DRIVER: &str = r#"
fn three() -> string effects[alloc, state] =
  let s: string = string_with_capacity(3) in
  let a: unit = string_push(s, 10) in
  let b: unit = string_push(s, 20) in
  let c: unit = string_push(s, 30) in
  s

fn overwrite_middle() -> i64 effects[alloc, state] =
  let s: string = three() in
  let w: unit = string_set(s, 1, 99) in
  to_i64(string_get(s, 0)) * 10000 + to_i64(string_get(s, 1)) * 100 + to_i64(string_get(s, 2))

fn len_unchanged() -> i64 effects[alloc, state] =
  let s: string = three() in
  let a: unit = string_set(s, 0, 1) in
  let b: unit = string_set(s, 1, 2) in
  let c: unit = string_set(s, 2, 3) in
  string_len(s)

fn last_write_wins() -> i64 effects[alloc, state] =
  let s: string = three() in
  let a: unit = string_set(s, 0, 5) in
  let b: unit = string_set(s, 0, 7) in
  to_i64(string_get(s, 0))

fn set_first_last() -> i64 effects[alloc, state] =
  let s: string = three() in
  let a: unit = string_set(s, 0, 5) in
  let b: unit = string_set(s, 2, 5) in
  to_i64(string_get(s, 0)) * 10000 + to_i64(string_get(s, 1)) * 100 + to_i64(string_get(s, 2))

fn zeroed(n: i64) -> string effects[alloc, state] =
  loop buf = string_with_capacity(n) while string_len(buf) < n do
    let pushed: unit = string_push(buf, 0) in buf

fn set_range(s: string, i: i64, n: i64) -> string effects[state] =
  if i >= n then s
  else
    let w: unit = string_set(s, i, to_u8(i * 7)) in
    set_range(s, i + 1, n)

fn sum_range(s: string, i: i64, n: i64, acc: i64) -> i64 effects[state] =
  if i >= n then acc
  else
    let b: i64 = to_i64(string_get(s, i)) in
    sum_range(s, i + 1, n, acc + b)

fn fill_then_sum() -> i64 effects[alloc, state] =
  let zeros: string = zeroed(8) in
  let filled: string = set_range(zeros, 0, 8) in
  sum_range(filled, 0, 8, 0)
"#;

#[test]
fn string_set_writes_run_native_and_match_eval() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("string-set.sqlite");
    let driver = temp.path().join("string_set_driver.cdb");
    std::fs::write(&driver, DRIVER).unwrap();

    run(&["init", path(&db)]);
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

    // `string_set` is a real lowered op (not desugared away), alongside the
    // push/get it complements.
    let ops = op_names(&db, "overwrite_middle");
    assert_eq!(
        ops.iter().filter(|op| op.as_str() == "string_set").count(),
        1,
        "overwrite_middle lowers exactly one string_set: {ops:?}"
    );
    let fill_ops = op_names(&db, "set_range");
    assert!(
        fill_ops.iter().any(|op| op == "string_set"),
        "set_range should lower a string_set: {fill_ops:?}"
    );

    if can_build_default_native_target() {
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
        assert_eq!(report["status"], "passed", "native string_set report: {report}");
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["unsupported"], 0);
    }
}

#[test]
fn string_set_projection_round_trips_to_a_fixpoint() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("string-set-rt.sqlite");
    let rebuilt = temp.path().join("string-set-rt2.sqlite");
    let driver = temp.path().join("string_set_rt.cdb");
    let projection = temp.path().join("string-set.export.cdb");
    std::fs::write(&driver, DRIVER).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&driver)]);

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let exported = std::fs::read_to_string(&projection).unwrap();
    assert!(
        exported.contains("string_set(s, i, to_u8(i * 7))"),
        "string_set call round-trips: {exported}"
    );

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    run(&["verify", path(&rebuilt)]);
    let reexport = temp.path().join("string-set.export2.cdb");
    run(&[
        "export",
        path(&rebuilt),
        "--branch",
        "main",
        "--out",
        path(&reexport),
    ]);
    assert_eq!(
        std::fs::read_to_string(&projection).unwrap(),
        std::fs::read_to_string(&reexport).unwrap(),
        "string_set projection is byte-stable"
    );
    assert_eq!(
        root_hash(&db),
        root_hash(&rebuilt),
        "import->export->import is a fixpoint for a string_set program"
    );
    assert_eq!(run(&["eval", path(&rebuilt), "fill_then_sum"]).trim(), "196");
}

#[test]
fn string_set_out_of_bounds_is_an_eval_error() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("string-set-oob.sqlite");
    let driver = temp.path().join("string_set_oob.cdb");
    std::fs::write(
        &driver,
        r#"
fn oob() -> i64 effects[alloc, state] =
  let s: string = string_with_capacity(2) in
  let a: unit = string_push(s, 1) in
  let w: unit = string_set(s, 1, 9) in
  to_i64(string_get(s, 0))
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&driver)]);
    // Index 1 is past len (len is 1 after one push; capacity is irrelevant):
    // the write is bounds-checked against len, exactly like string_get.
    let stderr = run_failure(&["eval", path(&db), "oob"]);
    assert!(
        stderr.contains("string_set index 1 out of bounds"),
        "expected out-of-bounds eval error, got: {stderr}"
    );
}

#[test]
fn string_set_rejects_malformed_forms() {
    // (label, program, expected diagnostic substring)
    let cases = [
        (
            "wrong-arg-count",
            "fn bad() -> i64 effects[alloc, state] =\n  let s: string = string_with_capacity(1) in\n  let w: unit = string_set(s, 0) in 0\n",
            "expects 3 args",
        ),
        (
            "non-string-target",
            "fn bad() -> i64 =\n  let x: i64 = 4 in\n  let w: unit = string_set(x, 0, 1) in 0\n",
            "string_set target must be string",
        ),
        (
            "non-place-target",
            "fn bad() -> i64 effects[alloc, state] =\n  let w: unit = string_set(string_with_capacity(1), 0, 1) in 0\n",
            "string_set target must be a mutable string place",
        ),
        (
            "non-i64-index",
            "fn bad() -> i64 effects[alloc, state] =\n  let s: string = string_with_capacity(1) in\n  let w: unit = string_set(s, true, 1) in 0\n",
            "string_set index",
        ),
        (
            "non-u8-value",
            "fn bad() -> i64 effects[alloc, state] =\n  let s: string = string_with_capacity(1) in\n  let v: i64 = 9 in\n  let w: unit = string_set(s, 0, v) in 0\n",
            "string_set value",
        ),
    ];

    for (label, program, expected) in cases {
        let temp = tempdir().unwrap();
        let db = temp.path().join(format!("reject-{label}.sqlite"));
        let source = temp.path().join(format!("reject-{label}.cdb"));
        std::fs::write(&source, program).unwrap();
        run(&["init", path(&db)]);
        let stderr = run_failure(&["import", path(&db), path(&source)]);
        assert!(
            stderr.contains(expected),
            "{label}: expected {expected:?}, got: {stderr}"
        );
    }
}
