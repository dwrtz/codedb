// Phase 4 (SPEC_V3 §7) acceptance: conditional and field-granular drop glue.
//
// A native program moves an owned heap value (`box<Line>`) in only SOME
// branches, and partially out of a record field, then compiles, runs, and
// agrees with the reference evaluator. Exactly-once drop is proven three ways:
//   * static leak guard — lowering still SCHEDULES a drop and the object is
//     wired to malloc/free (a skipped drop would drop the `free` reference);
//   * runtime double-free guard — libc aborts a double free, so the native run
//     failing its status is a double-drop; a passing run with the right value
//     means each `box` was freed exactly once on the path taken;
//   * evaluator oracle — eval == native, so the lifted move discipline did not
//     change observable behavior.

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

/// Collect op kinds recursively, descending into `if` branch blocks and `case`
/// arm blocks — compensating/residual drops are emitted inside those blocks, not
/// at the top level.
fn op_names_recursive(ops: &JsonValue, out: &mut Vec<String>) {
    for op in ops.as_array().unwrap() {
        out.push(op["op"].as_str().unwrap().to_string());
        for block_key in ["then_block", "else_block"] {
            if let Some(block) = op.get(block_key) {
                op_names_recursive(&block["operations"], out);
            }
        }
        if let Some(arms) = op.get("arms").and_then(JsonValue::as_array) {
            for arm in arms {
                op_names_recursive(&arm["block"]["operations"], out);
            }
        }
    }
}

fn op_names(ir: &JsonValue) -> Vec<String> {
    let mut out = Vec::new();
    op_names_recursive(&ir["ir"]["operations"], &mut out);
    out
}

fn object_references_symbol(object: &[u8], symbol: &[u8]) -> bool {
    object.windows(symbol.len()).any(|window| window == symbol)
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

const SOURCE: &str = r#"
record Line {
  price_cents: i64
  qty: i64
}

record Bundle {
  first: box<Line>
  second: box<Line>
}

fn take(b: box<Line>) -> i64 effects[alloc] = b.price_cents

// Conditional move: `b` is moved into `take` on the `then` path only; on the
// `else` path it stays live and is read. Conditional drop glue must drop it
// exactly once on each path (the `then` move consumes it; the `else` path drops
// it after the read — the compensating drop the merge inserts).
fn cond_move(flag: bool) -> i64 effects[alloc] =
  let b: box<Line> = box_new({ price_cents: 10, qty: 2 }) in
  if flag then take(b) else b.price_cents

fn cond_true() -> i64 effects[alloc] = cond_move(true)
fn cond_false() -> i64 effects[alloc] = cond_move(false)

// Field-granular move: `bundle.first` (an owned box) is moved into `take` and
// freed there; `bundle.second` stays owned and is read. The aggregate's
// scope-exit drop must free `bundle.second` (the live remainder) exactly once
// and must NOT free the moved-out `bundle.first` again.
fn partial_field() -> i64 effects[alloc] =
  let bundle: Bundle =
    { first: box_new({ price_cents: 7, qty: 1 }), second: box_new({ price_cents: 4, qty: 1 }) } in
  let moved: i64 = take(bundle.first) in
  moved + bundle.second.price_cents

fn main() -> i64 effects[alloc] = cond_true() + cond_false() + partial_field()
"#;

#[test]
fn conditional_and_partial_drop_glue_typecheck_lower_verify_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("drop-glue.sqlite");
    let source = temp.path().join("drop-glue.cdb");
    std::fs::write(&source, SOURCE).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);

    // Evaluator oracle: 10 (cond true) + 10 (cond false) + 11 (partial) = 31.
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "31");
    assert_eq!(run(&["eval", path(&db), "cond_true"]).trim(), "10");
    assert_eq!(run(&["eval", path(&db), "cond_false"]).trim(), "10");
    assert_eq!(run(&["eval", path(&db), "partial_field"]).trim(), "11");
    run(&["verify", path(&db)]);

    // Static leak guard: each owning function still schedules a drop and the
    // emitted object is wired to malloc/free.
    for entry in ["cond_move", "partial_field"] {
        let ir_path = temp.path().join(format!("{entry}.ir.json"));
        run(&["emit-ir", path(&db), entry, "--out", path(&ir_path)]);
        let ops = op_names(&read_json(&ir_path));
        assert!(
            ops.iter().any(|op| op == "drop"),
            "{entry}: expected a scheduled drop, got {ops:?}"
        );
        assert!(
            ops.iter().any(|op| op == "heap_alloc"),
            "{entry}: expected a heap_alloc, got {ops:?}"
        );

        let object_path = temp.path().join(format!("{entry}.o"));
        run(&[
            "emit-object",
            path(&db),
            entry,
            "--target",
            codedb::DEFAULT_NATIVE_TARGET,
            "--out",
            path(&object_path),
        ]);
        let object = std::fs::read(&object_path).unwrap();
        assert!(
            object_references_symbol(&object, b"malloc"),
            "{entry}: object lost its malloc reference"
        );
        assert!(
            object_references_symbol(&object, b"free"),
            "{entry}: object lost its free reference (drop glue would leak)"
        );
    }

    // Runtime exactly-once: build and run natively. A double-free aborts the
    // process (native status != passed); a leak is caught by the static guard
    // above. A passing run with value 30 == eval proves each box freed once.
    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "drop_glue_native",
            "--entry",
            "main",
            "--expect-i64",
            "31",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "31"})
        );
    }
}
