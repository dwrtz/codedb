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
fn box_values_typecheck_lower_verify_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-values.sqlite");
    let source = Path::new("examples/v2/box_heap.cdb");
    let projection = temp.path().join("box-values.export.cdb");
    let boxed_total_ir_path = temp.path().join("boxed-total.ir.json");
    let boxed_borrow_ir_path = temp.path().join("boxed-borrow.ir.json");
    let layout_path = temp.path().join("box-line.layout.json");
    let object_path = temp.path().join("boxed-total.o");

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "114");
    run(&["verify", path(&db)]);

    run(&[
        "emit-type-layout",
        path(&db),
        "box<Line>",
        "--out",
        path(&layout_path),
    ]);
    let layout = read_json(&layout_path);
    assert_eq!(layout["kind"], "box");
    assert_eq!(layout["copy_kind"], "move_only");
    assert_eq!(layout["drop_kind"], "needs_drop");
    assert_eq!(layout["contains_box"], true);
    assert_eq!(layout["abi"]["pass"], "by_value");
    assert_eq!(layout["abi"]["return"], "by_value");

    run(&[
        "emit-ir",
        path(&db),
        "boxed_total",
        "--out",
        path(&boxed_total_ir_path),
    ]);
    let boxed_total_ir = read_json(&boxed_total_ir_path);
    let ops = op_names(&boxed_total_ir);
    assert!(ops.contains(&"heap_alloc".to_string()));
    assert!(ops.contains(&"deref_box".to_string()));
    assert!(ops.contains(&"drop".to_string()));
    assert!(
        boxed_total_ir["ir"]["debug_map"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["lowered_op_kind"] == "heap_alloc")
    );

    run(&[
        "emit-ir",
        path(&db),
        "boxed_borrow",
        "--out",
        path(&boxed_borrow_ir_path),
    ]);
    let boxed_borrow_ir = read_json(&boxed_borrow_ir_path);
    let borrow_ops = op_names(&boxed_borrow_ir);
    assert!(borrow_ops.contains(&"deref_box".to_string()));
    assert!(borrow_ops.contains(&"borrow_shared".to_string()));

    run(&[
        "emit-object",
        path(&db),
        "boxed_total",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    assert_native_object_magic(&std::fs::read(&object_path).unwrap());
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
    assert!(exported.contains("box<Line>"));
    assert!(exported.contains("box<Node>"));
    assert!(exported.contains("box_new"));
    assert!(exported.contains("line_total(&'a b)"));

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "box_heap_native",
            "--entry",
            "main",
            "--expect-i64",
            "114",
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
            json!({"kind": "i64", "value": "114"})
        );
    }
}

#[test]
fn moving_box_prevents_use_after_move() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-use-after-move.sqlite");
    let source = temp.path().join("box-use-after-move.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn bad() -> i64 effects[alloc] =
  let b: box<Line> = box_new({ price_cents: 7, qty: 6 }) in
  let c: box<Line> = b in
  b.price_cents + c.qty
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    let stderr = run_fail(&["import", path(&db), path(&source)]);
    assert!(
        stderr.contains("move") || stderr.contains("moved"),
        "{stderr}"
    );
}

#[test]
fn moving_box_with_inner_mutable_borrow_keeps_loan_live() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-inner-mut-loan.sqlite");
    let source = temp.path().join("box-inner-mut-loan.cdb");

    std::fs::write(
        &source,
        r#"
record Editor<'a> {
  r: &'a mut i64
}

fn bad<'a>() -> i64 effects[alloc, state] =
  let y: i64 = 0 in
  let b: box<Editor<'a>> = box_new({ r: &'a mut y }) in
  let c: box<Editor<'a>> = b in
  let write_y: unit = y = 5 in
  y
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    let stderr = run_fail(&["import", path(&db), path(&source)]);
    assert!(
        stderr.contains("bad_borrow") || stderr.contains("live mutable borrow"),
        "{stderr}"
    );
}

#[test]
fn box_new_builds_named_record_value_in_destination_layout() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-named-record-layout.sqlite");
    let source = temp.path().join("box-named-record-layout.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

record Holder {
  tag: i64
  line: box<Line>
}

fn nested_box() -> i64 effects[alloc] =
  let h: box<Holder> = box_new({ tag: 1, line: box_new({ price_cents: 7 }) }) in
  h.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    assert_eq!(run(&["eval", path(&db), "nested_box"]).trim(), "7");

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "nested_box_native",
            "--entry",
            "nested_box",
            "--expect-i64",
            "7",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["native_mismatches"], 0);
    }
}

#[test]
fn box_return_values_compile_and_run_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-return.sqlite");
    let source = temp.path().join("box-return.cdb");
    let make_box_object = temp.path().join("make-box.o");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

fn make_box() -> box<Line> effects[alloc] =
  box_new({ price_cents: 7 })

fn main() -> i64 effects[alloc] =
  let b: box<Line> = make_box() in
  b.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "7");

    run(&[
        "emit-object",
        path(&db),
        "make_box",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&make_box_object),
    ]);
    assert_native_object_magic(&std::fs::read(&make_box_object).unwrap());

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "box_return_native",
            "--entry",
            "main",
            "--expect-i64",
            "7",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["native_mismatches"], 0);
    }
}

#[test]
fn assigning_over_box_drops_previous_value() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-reassign.sqlite");
    let source = temp.path().join("box-reassign.cdb");
    let ir_path = temp.path().join("box-reassign.ir.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

fn reassign_box() -> i64 effects[alloc, state] =
  let b: box<Line> = box_new({ price_cents: 1 }) in
  let ignored: unit = b = box_new({ price_cents: 2 }) in
  b.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    assert_eq!(run(&["eval", path(&db), "reassign_box"]).trim(), "2");

    run(&[
        "emit-ir",
        path(&db),
        "reassign_box",
        "--out",
        path(&ir_path),
    ]);
    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert_eq!(
        ops.iter().filter(|op| op.as_str() == "heap_alloc").count(),
        2
    );
    assert_eq!(ops.iter().filter(|op| op.as_str() == "drop").count(), 2);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "reassign_box_native",
            "--entry",
            "reassign_box",
            "--expect-i64",
            "2",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["native_mismatches"], 0);
    }
}

#[test]
fn recursive_box_payloads_drop_natively() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-recursive-drop.sqlite");
    let source = temp.path().join("box-recursive-drop.cdb");

    std::fs::write(
        &source,
        r#"
enum Node {
  empty: unit
  next: box<Node>
}

fn recursive_next() -> i64 effects[alloc] =
  let n: box<Node> = box_new(Node::next(box_new(Node::empty(())))) in
  let moved: box<Node> = n in
  5
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    assert_eq!(run(&["eval", path(&db), "recursive_next"]).trim(), "5");

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "recursive_next_native",
            "--entry",
            "recursive_next",
            "--expect-i64",
            "5",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["native_mismatches"], 0);
    }
}

#[test]
fn box_new_requires_alloc_effect() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-missing-alloc.sqlite");
    let source = temp.path().join("box-missing-alloc.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

fn bad() -> i64 =
  let b: box<Line> = box_new({ price_cents: 7, qty: 6 }) in
  b.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    let stderr = run_fail(&["import", path(&db), path(&source)]);
    assert!(stderr.contains("alloc"), "{stderr}");
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

fn object_references_symbol(object: &[u8], symbol: &[u8]) -> bool {
    object.windows(symbol.len()).any(|window| window == symbol)
}

/// Leak-regression guard (static / structural). The value oracle and the
/// runtime double-free abort exercised by the other box tests catch wrong
/// values and double-frees, but neither catches a pure *leak* — an allocation
/// with no matching free, which is the shape a future skipped-drop regression
/// would take. This guard asserts, for every owned-box shape, that (a) lowering
/// still SCHEDULES a `drop` for the owned slot and (b) the emitted native object
/// is still WIRED to `malloc` and `free` (drop glue reaches the platform free
/// symbol). A regression that drops the drop op, misclassifies the box as Copy,
/// or no-ops the drop glue removes the `free` reference and trips this test. The
/// no-box control proves the guard discriminates rather than always passing.
/// (emit-ir/emit-object are pure codegen, so this runs without a host linker.)
#[test]
fn box_owning_functions_schedule_drops_and_emit_free() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-leak-guard.sqlite");
    let source = Path::new("examples/v2/box_heap.cdb");
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(source)]);

    // (entry, owns_heap)
    for (entry, owns_heap) in [
        ("boxed_total", true),    // single box dropped at scope end
        ("boxed_borrow", true),   // box borrowed then dropped
        ("move_box", true),       // box moved; exactly the new owner is freed
        ("recursive_node", true), // recursive enum Node: drop must recurse and free every node
        ("line_total", false),    // no box: must not drop and must not reference malloc/free
    ] {
        let ir_path = temp.path().join(format!("{entry}.ir.json"));
        run(&["emit-ir", path(&db), entry, "--out", path(&ir_path)]);
        let drops = op_names(&read_json(&ir_path))
            .iter()
            .filter(|op| op.as_str() == "drop")
            .count();

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
        let references_malloc = object_references_symbol(&object, b"malloc");
        let references_free = object_references_symbol(&object, b"free");

        if owns_heap {
            assert!(
                drops >= 1,
                "{entry}: expected a scheduled drop for the owned box slot, found none (would leak)"
            );
            assert!(
                references_malloc,
                "{entry}: emitted object does not reference malloc (allocation lost)"
            );
            assert!(
                references_free,
                "{entry}: emitted object does not reference free (drop glue would leak)"
            );
        } else {
            assert_eq!(
                drops, 0,
                "{entry}: no-box control unexpectedly scheduled a drop"
            );
            assert!(
                !references_malloc && !references_free,
                "{entry}: no-box control unexpectedly references malloc/free"
            );
        }
    }
}
