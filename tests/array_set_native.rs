// Phase 9 follow-on (R9): `array_set(arr, i, v)` — functional update of one element
// of a fixed Copy array, yielding a NEW `array<T, N>` equal to `arr` with element
// `i` set to `v`. It is the array counterpart of `string_push`/`vec_push` for a Copy
// buffer and the substrate for a worklist that builds an array by index (e.g. the
// SHA-256 message schedule). The element type must be a non-reference Copy value with
// trivial drop (so the array is Copy, a `loop` can carry it, and overwriting element
// `i` is leak-free). The index is bounds-checked at runtime; a literal out-of-range
// index is rejected at type-check. Lowering reuses the array-init + bounds-check +
// indexed-store machinery — no new lowered op, no new backend codegen. Oracle: eval ==
// native; the `array_set(..)` call round-trips through the `.cdb` projection.
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

fn ops(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect()
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

// Functions are written in the projection's canonical (alphabetical) order so that
// import->export->import is a root-hash fixpoint. (The projection sorts symbols by
// name while birth seeds derive from source position, a pre-existing property of the
// checked-view round-trip unrelated to array_set; ordering the source canonically
// keeps the fixpoint assertion below meaningful.)
const SOURCE: &str = r#"
record Sched { arr: array<i64, 8>
  i: i64 }

record U32Sched { buf: array<u32, 4>
  i: i64 }

// A worklist that BUILDS an array by a RUNTIME index: the Copy array is the loop
// accumulator, updated in place each iteration via array_set. sum of squares 0..8.
fn build_squares() -> array<i64, 8> =
  let done: Sched =
    loop s = { arr: [0; 8], i: 0 } while s.i < 8 do
      { arr: array_set(s.arr, s.i, s.i * s.i), i: s.i + 1 }
  in done.arr

// A non-loop array_set over a RUNTIME index `i` (a parameter), so its bounds check
// appears at the top level of the lowered IR.
fn set_at(xs: array<i64, 4>, i: i64) -> array<i64, 4> = array_set(xs, i, 9)

// Chained functional updates over a literal index: each `array_set` yields a fresh
// array; the result reads back the overwritten slots and the untouched ones.
fn set_chain() -> i64 =
  let a: array<i64, 4> = [0; 4] in
  let b: array<i64, 4> = array_set(a, 1, 100) in
  let c: array<i64, 4> = array_set(b, 3, 7) in
  c[0] + c[1] + c[2] + c[3]

fn set_loop() -> i64 =
  let sq: array<i64, 8> = build_squares() in
  sq[0] + sq[1] + sq[2] + sq[3] + sq[4] + sq[5] + sq[6] + sq[7]

fn set_runtime() -> i64 =
  let a: array<i64, 4> = [0; 4] in
  let b: array<i64, 4> = set_at(a, 2) in
  b[2]

// A u32 (sized-int, distinct layout) array built by a runtime index with wrapping
// arithmetic — the codec/hash-schedule shape. buf[i] = i*1000 for i in 0..4.
fn set_u32() -> i64 =
  let done: U32Sched =
    loop s = { buf: [0x0; 4], i: 0 } while s.i < 4 do
      { buf: array_set(s.buf, s.i, to_u32(s.i) * 1000), i: s.i + 1 }
  in to_i64(done.buf[3])
"#;

/// (entry, expected i64) — natively runnable in-frame cases.
const CASES: &[(&str, &str)] = &[
    ("set_chain", "107"),   // 0 + 100 + 0 + 7
    ("set_loop", "140"),    // 0+1+4+9+16+25+36+49
    ("set_u32", "3000"),    // buf[3] = 3 * 1000
    ("set_runtime", "9"),   // b[2] set to 9 via a runtime-index array_set
];

#[test]
fn array_set_lowers_runs_native_and_round_trips() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("array-set.sqlite");
    let source = temp.path().join("array-set.cdb");
    let projection = temp.path().join("array-set.export.cdb");
    let rebuilt = temp.path().join("array-set-rebuilt.sqlite");
    std::fs::write(&source, SOURCE).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);

    // Reference-evaluator oracle for every case.
    for (entry, expected) in CASES {
        assert_eq!(
            run(&["eval", path(&db), entry]).trim(),
            *expected,
            "eval mismatch for {entry}"
        );
    }

    // Lowering shape: `array_set` over a runtime index lowers to a whole-array copy
    // (Store of the source array), a bounds check, then the indexed element store —
    // reusing the array-init + bounds-check + AddrOfIndex/Store machinery (no new op).
    let ir_path = temp.path().join("set_at.ir.json");
    run(&["emit-ir", path(&db), "set_at", "--out", path(&ir_path)]);
    let set_at_ops = ops(&read_json(&ir_path));
    assert!(
        set_at_ops.iter().any(|op| op == "bounds_check"),
        "a runtime-index array_set bounds-checks the index: {set_at_ops:?}"
    );
    assert!(
        set_at_ops.iter().any(|op| op == "addr_of_index"),
        "array_set addresses the element slot: {set_at_ops:?}"
    );

    // A literal-index array_set needs NO bounds check (the index is in range by
    // type-check), like a constant array_index.
    let chain_ir = temp.path().join("chain.ir.json");
    run(&["emit-ir", path(&db), "set_chain", "--out", path(&chain_ir)]);
    let chain_ops = ops(&read_json(&chain_ir));
    assert!(
        !chain_ops.iter().any(|op| op == "bounds_check"),
        "a literal-index array_set is not bounds-checked: {chain_ops:?}"
    );

    // The `array_set(..)` call survives the projection (a checked view) and the whole
    // program round-trips import -> export -> import to a byte-stable fixpoint.
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
        exported.contains("array_set(s.arr, s.i, s.i * s.i)"),
        "array_set call round-trips: {exported}"
    );

    run(&["init", path(&rebuilt)]);
    run(&["import", path(&rebuilt), path(&projection)]);
    run(&["verify", path(&rebuilt)]);
    let reexport = temp.path().join("array-set.export2.cdb");
    run(&["export", path(&rebuilt), "--branch", "main", "--out", path(&reexport)]);
    assert_eq!(
        std::fs::read_to_string(&projection).unwrap(),
        std::fs::read_to_string(&reexport).unwrap(),
        "array_set projection is byte-stable"
    );
    assert_eq!(
        root_hash(&db),
        root_hash(&rebuilt),
        "import->export->import is a fixpoint for an array_set program"
    );
    assert_eq!(run(&["eval", path(&rebuilt), "set_loop"]).trim(), "140");

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
        assert_eq!(report["status"], "passed", "native array_set report: {report}");
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["unsupported"], 0);
    }
}

#[test]
fn array_set_rejects_unsound_or_malformed_forms() {
    // (label, program, expected diagnostic substring)
    let cases = [
        (
            "move-only-element",
            "fn bad(s: string) -> i64 effects[alloc, state] =\n  let xs: array<string, 2> = [string_new(\"x\"), string_new(\"y\")] in\n  let ys: array<string, 2> = array_set(xs, 0, s) in 0\n",
            "non-reference Copy",
        ),
        (
            "value-type-mismatch",
            "fn bad() -> i64 =\n  let a: array<i64, 3> = [0; 3] in array_set(a, 0, true)[0]\n",
            "does not match element type",
        ),
        (
            "literal-index-out-of-bounds",
            "fn bad() -> i64 =\n  let a: array<i64, 3> = [0; 3] in array_set(a, 5, 9)[0]\n",
            "out of bounds",
        ),
        (
            "non-array-target",
            "fn bad() -> i64 = array_set(7, 0, 9)\n",
            "must be a fixed array",
        ),
        (
            "wrong-arg-count",
            "fn bad() -> i64 =\n  let a: array<i64, 3> = [0; 3] in array_set(a, 0)[0]\n",
            "expects 3 args",
        ),
        (
            // Indexing requires an addressable place; the array_set RESULT is
            // an rvalue, so `array_set(a, i, v)[j]` is rejected fail-closed
            // (bind it with a `let` first). Pinned so the envelope is explicit.
            "rvalue-indexing",
            "fn bad() -> i64 =\n  let a: array<i64, 3> = [0; 3] in array_set(a, 0, 9)[0]\n",
            "not an addressable place",
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
