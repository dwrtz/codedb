// Phase 8 (rung 0) substrate: the CIR artifact — a flat binary serialization of
// an entry's lowered-IR closure, the input format for the CodeDB-hosted
// reference evaluator. `emit-cir` carries a built-in honesty gate: it decodes
// its own output and fails unless the decoded `LoweredFunctionIr`s are
// structurally identical to the lowered originals, so every successful emission
// in this file IS a proven round trip. These tests pin the rest: byte
// determinism (within one database and across an independent rebuild from the
// same source — deterministic birth identities make the roots, the IR, and
// therefore the CIR bytes reproduce), closure shape, and the fail-closed
// rejection of external functions (the reference evaluator cannot execute
// externs, so rung-0 CIR refuses to encode them).
use std::path::Path;

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

fn repo_file(relative: &str) -> String {
    format!("{}/{relative}", env!("CARGO_MANIFEST_DIR"))
}

/// One driver exercising every lowered-op family in a single closure: generic
/// record/enum instantiation (a monomorphic instance symbol in the table),
/// box/unbox, enum construction + `case`, vec ops, all five string ops
/// (including `string_set`), static bytes + slice indexing, argv builtins,
/// recursion with early return, fold, and loop.
const KITCHEN_SINK: &str = r#"
record Pair<T> {
  first: T
  second: T
}

enum Opt<T> {
  none: unit
  some: T
}

fn pick<T>(p: Pair<T>, want_first: bool) -> T =
  if want_first then p.first else p.second

fn boxed_sum(n: i64) -> i64 effects[alloc] =
  let b: box<i64> = box_new(n) in
  unbox(b) + 1

fn opt_value(o: Opt<i64>) -> i64 =
  case o of
    none(u) => 0 - 1
  | some(v) => v

fn vec_demo() -> i64 effects[alloc, state] =
  let v: vec<i64> = vec_new(4) in
  let a: unit = vec_push(v, 11) in
  let b: unit = vec_push(v, 22) in
  vec_get(v, 0) + vec_get(v, 1) + vec_len(v)

fn str_demo() -> i64 effects[alloc, state] =
  let s: string = string_new("hi") in
  let t: string = string_with_capacity(2) in
  let p: unit = string_push(t, string_get(s, 0)) in
  let w: unit = string_set(t, 0, 122) in
  string_len(s) + to_i64(string_get(t, 0))

fn slice_demo() -> i64 =
  let bytes: slice<'static, u8> = b"abc" in
  to_i64(bytes[1]) + len(bytes)

fn args_demo() -> i64 effects[io] =
  arg_count() * 10 + arg_len(0)

fn scan(i: i64, acc: i64) -> i64 =
  if i >= 4 then acc
  else if acc > 100 then return 0 - acc
  else scan(i + 1, acc * 3 + i)

fn fold_demo() -> i64 =
  let xs: array<i64, 4> = [2; 4] in
  fold x in xs with acc = 0 do acc + x

fn loop_demo() -> i64 =
  loop n = 1 while n < 50 do n * 2

fn main() -> i64 effects[alloc, state, io] =
  let p: Pair<i64> = { first: 3, second: 4 } in
  pick(p, true)
    + boxed_sum(5)
    + opt_value(Opt::some(7))
    + opt_value(Opt::none)
    + vec_demo()
    + str_demo()
    + slice_demo()
    + args_demo()
    + scan(0, 1)
    + fold_demo()
    + loop_demo()
"#;

#[test]
fn cir_round_trips_and_is_deterministic_for_the_corpus() {
    // (label, source path or inline, entry)
    let temp = tempdir().unwrap();
    let kitchen = temp.path().join("kitchen.cdb");
    std::fs::write(&kitchen, KITCHEN_SINK).unwrap();
    let cases: &[(&str, String, &str)] = &[
        ("fnv1a", repo_file("examples/fnv1a.cdb"), "main"),
        ("sha256", repo_file("examples/v3/sha256.cdb"), "digest_0"),
        (
            "tokenizer",
            repo_file("examples/v3/tokenizer.cdb"),
            "tokenize_ok",
        ),
        (
            "kitchen-sink",
            kitchen.to_str().expect("utf8 path").to_string(),
            "main",
        ),
    ];

    for (label, source, entry) in cases {
        let db = temp.path().join(format!("{label}.sqlite"));
        let rebuilt = temp.path().join(format!("{label}-rebuilt.sqlite"));
        let first = temp.path().join(format!("{label}.cir"));
        let second = temp.path().join(format!("{label}-2.cir"));
        let independent = temp.path().join(format!("{label}-rebuilt.cir"));

        run(&["init", path(&db)]);
        run(&["import", path(&db), source]);

        // Every successful emission has already decode-verified itself against
        // the lowered IR (the built-in honesty gate).
        let summary = parse_json(&run(&[
            "emit-cir",
            path(&db),
            entry,
            "--out",
            path(&first),
            "--json",
        ]));
        assert_eq!(summary["schema"], "codedb/cir/v0", "{label} schema");
        assert!(
            summary["function_count"].as_u64().unwrap() >= 1,
            "{label} function count"
        );
        let first_bytes = std::fs::read(&first).unwrap();
        assert_eq!(
            first_bytes.len() as u64,
            summary["byte_len"].as_u64().unwrap(),
            "{label} byte_len"
        );

        // Re-emission from the same database is byte-identical.
        run(&["emit-cir", path(&db), entry, "--out", path(&second)]);
        assert_eq!(
            first_bytes,
            std::fs::read(&second).unwrap(),
            "{label}: re-emission must be byte-identical"
        );

        // An independent rebuild from the same source reaches the same bytes
        // (deterministic birth identities -> same root -> same lowered IR ->
        // same CIR).
        run(&["init", path(&rebuilt)]);
        run(&["import", path(&rebuilt), source]);
        run(&[
            "emit-cir",
            path(&rebuilt),
            entry,
            "--out",
            path(&independent),
        ]);
        assert_eq!(
            first_bytes,
            std::fs::read(&independent).unwrap(),
            "{label}: independent rebuild must reproduce the CIR bytes"
        );
    }
}

#[test]
fn cir_closure_includes_monomorphic_instances_not_templates() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("generic.sqlite");
    let driver = temp.path().join("generic.cdb");
    std::fs::write(&driver, KITCHEN_SINK).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&driver)]);

    let out = temp.path().join("generic.cir");
    let summary = parse_json(&run(&[
        "emit-cir",
        path(&db),
        "main",
        "--out",
        path(&out),
        "--json",
    ]));
    // 10 named non-generic functions reachable from main, plus exactly one
    // monomorphic instance (pick<i64>); the generic template itself does not
    // lower and must not appear.
    assert_eq!(
        summary["function_count"], 11,
        "closure = named fns + pick<i64> instance: {summary}"
    );
    let entry_index = summary["entry_index"].as_u64().unwrap() as usize;
    let functions = summary["functions"].as_array().unwrap();
    assert_eq!(
        functions[entry_index]["symbol"],
        summary["entry_symbol"],
        "entry index points at the entry symbol"
    );
    // The table is sorted by symbol hash (the canonical order).
    let symbols: Vec<&str> = functions
        .iter()
        .map(|f| f["symbol"].as_str().unwrap())
        .collect();
    let mut sorted = symbols.clone();
    sorted.sort_unstable();
    assert_eq!(symbols, sorted, "function table sorted by symbol hash");
}

#[test]
fn cir_rejects_external_functions_fail_closed() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("extern.sqlite");
    let driver = temp.path().join("extern.cdb");
    std::fs::write(
        &driver,
        r#"module std.platform {
extern fn write(fd: i64, ptr: raw_ptr<u8>, len: i64) -> i64 abi[c] effects[io, ffi, unsafe] link_name "write" library "c"
}

fn emit(bytes: slice<'static, u8>) -> i64 effects[io, ffi, unsafe] =
  std.platform.write(1, raw_ptr(&'static bytes[0]), len(bytes))

fn main() -> i64 effects[io, ffi, unsafe] = emit("hi")
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&driver)]);

    let out = temp.path().join("extern.cir");
    let stderr = run_failure(&["emit-cir", path(&db), "main", "--out", path(&out)]);
    assert!(
        stderr.contains("does not encode external functions"),
        "expected the extern rejection, got: {stderr}"
    );
    assert!(!out.exists(), "no artifact may be written on failure");
}
