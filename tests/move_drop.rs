use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
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

#[test]
fn shared_reference_records_are_copy_and_loan_ends_at_scope() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("shared-record-copy.sqlite");
    let source = temp.path().join("shared-record-copy.cdb");
    let ir_path = temp.path().join("main.ir.json");

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

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let observed: i64 =
    (let first: LineView<'a> = { line: &'a line } in
     let second: LineView<'a> = first in
     first.line.price_cents + second.line.qty) in
  line.price_cents + observed
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "54");
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "main", "--out", path(&ir_path)]);

    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert!(ops.contains(&"copy".to_string()));
    // LineView is Copy (shared reference), so it owns nothing and must not be
    // dropped. The loan ending at scope is a borrow-checker property, not a
    // lowered drop op.
    assert!(!ops.contains(&"drop".to_string()));

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "shared_record_copy_native",
            "--entry",
            "main",
            "--expect-i64",
            "54",
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
fn moving_mutable_reference_record_transfers_loan_and_drop_ends_it() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("mutable-record-move.sqlite");
    let source = temp.path().join("mutable-record-move.cdb");
    let ir_path = temp.path().join("main.ir.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let observed: i64 =
    (let editor: LineEditor<'a> = { line: &'a mut line } in
     let total: i64 =
       (let moved: LineEditor<'a> = editor in
        moved.line.price_cents) in
     line.price_cents + total) in
  observed
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "50");
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "main", "--out", path(&ir_path)]);

    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert!(ops.contains(&"move".to_string()));
    assert!(ops.contains(&"drop".to_string()));

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "mutable_record_move_native",
            "--entry",
            "main",
            "--expect-i64",
            "50",
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
fn moving_move_only_record_while_shared_borrow_is_live_is_rejected() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("move-while-shared-borrow.sqlite");
    let source = temp.path().join("move-while-shared-borrow.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25 } in
  let editor: LineEditor<'a> = { line: &'a mut line } in
  let shared: &'a LineEditor<'a> = &'a editor in
  let moved: LineEditor<'a> = editor in
  0
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_borrow"))
        .stderr(predicate::str::contains("move of"))
        .stderr(predicate::str::contains("live"))
        .stderr(predicate::str::contains("borrow"));
}

#[test]
fn moving_mutable_reference_record_through_if_transfers_loan() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("mutable-record-if-move.sqlite");
    let source = temp.path().join("mutable-record-if-move.cdb");
    let ir_path = temp.path().join("main.ir.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25 } in
  let editor: LineEditor<'a> = { line: &'a mut line } in
  let moved: LineEditor<'a> =
    (if true then editor else editor) in
  moved.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "25");
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "main", "--out", path(&ir_path)]);

    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert!(ops.contains(&"if".to_string()));
    assert!(
        serde_json::to_string(&ir)
            .unwrap()
            .contains("\"op\":\"move\"")
    );

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "mutable_record_if_move_native",
            "--entry",
            "main",
            "--expect-i64",
            "25",
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
fn moving_mutable_reference_record_into_call_ends_loan_after_return() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("call-consume-move.sqlite");
    let source = temp.path().join("call-consume-move.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn consume<'a>(editor: LineEditor<'a>) -> i64 = editor.line.price_cents

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let editor: LineEditor<'a> = { line: &'a mut line } in
  let observed: i64 = consume(editor) in
  line.price_cents + observed
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "50");
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "call_consume_move_native",
            "--entry",
            "main",
            "--expect-i64",
            "50",
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
fn enum_payload_cannot_return_local_borrow() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("enum-local-borrow.sqlite");
    let source = temp.path().join("enum-local-borrow.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

fn leak<'a>() -> enum { none: unit, some: &'a Line } =
  let line: Line = { price_cents: 25 } in
  enum { none: unit, some: &'a Line }::some(&'a line)
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
fn mutable_reference_record_parameter_gets_drop_scaffold() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("param-drop-scaffold.sqlite");
    let source = temp.path().join("param-drop-scaffold.cdb");
    let ir_path = temp.path().join("consume.ir.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn consume<'a>(editor: LineEditor<'a>) -> i64 = editor.line.price_cents

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25 } in
  let editor: LineEditor<'a> = { line: &'a mut line } in
  consume(editor)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["emit-ir", path(&db), "consume", "--out", path(&ir_path)]);
    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert!(ops.contains(&"drop".to_string()));
    run(&["verify", path(&db)]);
}

#[test]
fn moved_out_parameter_is_not_dropped() {
    // Regression for the move-unaware drop scaffold: a parameter whose whole
    // value is moved out (returned) must not also be dropped, or the lowered IR
    // would drop storage the caller now owns (a latent double-free once real
    // drop glue lands). The drop-once verifier also rejects a drop-after-move,
    // so `verify` would fail if the scaffold regressed.
    let temp = tempdir().unwrap();
    let db = temp.path().join("moved-param-no-drop.sqlite");
    let source = temp.path().join("moved-param-no-drop.cdb");
    let ir_path = temp.path().join("passthrough.ir.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn passthrough<'a>(editor: LineEditor<'a>) -> LineEditor<'a> = editor
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "passthrough", "--out", path(&ir_path)]);

    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert!(ops.contains(&"move".to_string()));
    assert!(!ops.contains(&"drop".to_string()));
}

#[test]
fn using_moved_move_only_record_is_rejected() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("use-after-move.sqlite");
    let source = temp.path().join("use-after-move.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let editor: LineEditor<'a> = { line: &'a mut line } in
  let moved: LineEditor<'a> = editor in
  editor.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_move"))
        .stderr(predicate::str::contains("use after move"));
}

#[test]
fn partial_move_of_a_field_through_a_box_is_rejected_cleanly() {
    // Moving a field reached through a `box` auto-deref (`h.inner` where
    // `h: box<Holder>`) is not field-granular-droppable; it must fail closed at the
    // checker with a clean `unsupported_move` diagnostic (suggesting `unbox`), never
    // crash lowering. Regression for the box-auto-deref place-tracking gap (SPEC_V3
    // §7) — the place flattens away the deref, so a naive lowering treated the box as
    // a record and panicked.
    let temp = tempdir().unwrap();
    let db = temp.path().join("box-deref-move.sqlite");
    let source = temp.path().join("box-deref-move.cdb");
    std::fs::write(
        &source,
        r#"
record Inner { x: i64 }
record Holder { inner: box<Inner> }

fn take(b: box<Inner>) -> i64 effects[alloc] = b.x

fn f() -> i64 effects[alloc] =
  let h: box<Holder> = box_new({ inner: box_new({ x: 5 }) }) in
  take(h.inner)

fn main() -> i64 effects[alloc] = f()
"#,
    )
    .unwrap();
    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported_move"))
        .stderr(predicate::str::contains("box deref"));
}

#[test]
fn moving_mutable_reference_record_out_of_inner_let_keeps_loan_live() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("inner-let-move-loan.sqlite");
    let source = temp.path().join("inner-let-move-loan.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let moved: LineEditor<'a> =
    (let editor: LineEditor<'a> = { line: &'a mut line } in
     editor) in
  line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_borrow"))
        .stderr(predicate::str::contains("shared read"))
        .stderr(predicate::str::contains("live mutable borrow"));
}

#[test]
fn shared_reference_record_cannot_outlive_inner_local_storage() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("shared-record-inner-local.sqlite");
    let source = temp.path().join("shared-record-inner-local.cdb");

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

fn main<'a>() -> i64 =
  let view: LineView<'a> =
    (let line: Line = { price_cents: 25, qty: 4 } in
     { line: &'a line }) in
  view.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_borrow"))
        .stderr(predicate::str::contains("outlives local storage"));
}

#[test]
fn mutable_reference_record_cannot_outlive_inner_local_storage() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("mutable-record-inner-local.sqlite");
    let source = temp.path().join("mutable-record-inner-local.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn main<'a>() -> i64 effects[state] =
  let editor: LineEditor<'a> =
    (let line: Line = { price_cents: 25, qty: 4 } in
     { line: &'a mut line }) in
  let changed: unit = editor.line.price_cents = 99 in
  editor.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_borrow"))
        .stderr(predicate::str::contains("outlives local storage"));
}

#[test]
fn assignment_cannot_smuggle_inner_mutable_borrow_out() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("assign-smuggle-inner-mut.sqlite");
    let source = temp.path().join("assign-smuggle-inner-mut.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn main<'a>() -> i64 effects[state] =
  let outer: Line = { price_cents: 1 } in
  let editor: LineEditor<'a> = { line: &'a mut outer } in
  let leaked: LineEditor<'a> =
    (let inner: Line = { price_cents: 2 } in
     let changed: unit = editor.line = &'a mut inner in
     editor) in
  leaked.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    bin()
        .args(["import", path(&db), path(&source)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bad_borrow"))
        .stderr(predicate::str::contains("outlives local storage"));
}

#[test]
fn moving_outer_mutable_reference_record_through_inner_let_transfers_loan() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("outer-move-through-inner-let.sqlite");
    let source = temp.path().join("outer-move-through-inner-let.cdb");
    let ir_path = temp.path().join("main.ir.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let editor: LineEditor<'a> = { line: &'a mut line } in
  let moved: LineEditor<'a> =
    (let marker: i64 = 1 in
     editor) in
  moved.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "25");
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "main", "--out", path(&ir_path)]);

    let ir = read_json(&ir_path);
    let ops = op_names(&ir);
    assert!(ops.contains(&"move".to_string()));
    assert!(ops.contains(&"drop".to_string()));
}

#[test]
fn asymmetric_conditional_move_through_if_is_accepted() {
    // Moving an owned (move-only) value in only one branch of an `if` is now
    // supported by conditional drop glue (SPEC_V3 §7): lowering emits a
    // compensating drop in the branch that left the value live, so each path
    // drops it exactly once. Previously rejected fail-closed; now imports,
    // verifies, and lowers through the lowered-IR drop verifier.
    let temp = tempdir().unwrap();
    let db = temp.path().join("asym-if-move.sqlite");
    let source = temp.path().join("asym-if-move.cdb");
    let ir_path = temp.path().join("pick.ir.json");

    std::fs::write(
        &source,
        r#"
record Line { price_cents: i64 }

record LineEditor<'a> { line: &'a mut Line }

fn pick<'a>(editor: LineEditor<'a>, other: LineEditor<'a>, choose: bool) -> i64 =
  let chosen: LineEditor<'a> = (if choose then editor else other) in
  chosen.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    // Lowering accepts the asymmetric move and passes the lowered-IR drop
    // verifier (no double-drop / no drop-after-move across the branches).
    let ir = read_json(&{
        run(&["emit-ir", path(&db), "pick", "--out", path(&ir_path)]);
        ir_path.clone()
    });
    // The compensating drop is emitted (mut-ref records take the drop scaffold).
    assert!(
        op_names(&ir).iter().any(|op| op == "drop"),
        "expected a compensating drop in the lowered IR, got {:?}",
        op_names(&ir)
    );
}

#[test]
fn partial_field_move_of_move_only_value_is_accepted() {
    // Moving a move-only value out of a record field is a partial move that
    // leaves the enclosing aggregate with a hole. Field-granular drop glue
    // (SPEC_V3 §7) drops the live remainder of the aggregate while skipping the
    // moved-out field, so this now imports, verifies, and lowers (previously
    // rejected fail-closed).
    let temp = tempdir().unwrap();
    let db = temp.path().join("partial-field-move.sqlite");
    let source = temp.path().join("partial-field-move.cdb");
    let ir_path = temp.path().join("main.ir.json");

    std::fs::write(
        &source,
        r#"
record Line { price_cents: i64 }

record LineEditor<'a> { line: &'a mut Line }

record Pair<'a> { ed: LineEditor<'a>, n: i64 }

fn take<'a>(editor: LineEditor<'a>) -> i64 = editor.line.price_cents

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25 } in
  let pair: Pair<'a> = { ed: { line: &'a mut line }, n: 7 } in
  let observed: i64 = take(pair.ed) in
  observed
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "main", "--out", path(&ir_path)]);
}

#[test]
fn partial_field_move_of_move_only_aggregate_into_let_is_accepted() {
    // The aggregate-init move path: `let c = container.cursor` where the moved
    // field is a move-only aggregate LARGER than a register (here Cursor is 16
    // bytes) routes through `lower_aggregate_place_init_to_address`. Field-granular
    // drop glue (SPEC_V3 §7) now tracks the partial move and drops the live
    // remainder of `container` (its `tag`) while skipping the moved-out cursor,
    // so the lowered-IR drop verifier confirms exactly-once (no double-drop).
    let temp = tempdir().unwrap();
    let db = temp.path().join("partial-agg-move-let.sqlite");
    let source = temp.path().join("partial-agg-move-let.cdb");
    let ir_path = temp.path().join("main.ir.json");

    std::fs::write(
        &source,
        r#"
record Line { price_cents: i64 }

record Cursor<'a> { line: &'a mut Line, pos: i64 }

record Container<'a> { cursor: Cursor<'a>, tag: i64 }

fn main<'a>() -> i64 =
  let line: Line = { price_cents: 25 } in
  let container: Container<'a> = { cursor: { line: &'a mut line, pos: 7 }, tag: 3 } in
  let c: Cursor<'a> = container.cursor in
  c.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "main", "--out", path(&ir_path)]);
}

#[test]
fn partial_field_move_of_move_only_aggregate_from_param_is_accepted() {
    // Same aggregate-init partial move, by-value-parameter form: moving a
    // move-only aggregate field out of a by-value record parameter. Field-granular
    // drop glue (SPEC_V3 §7) drops the live remainder of the parameter at
    // function end while skipping the moved-out cursor.
    let temp = tempdir().unwrap();
    let db = temp.path().join("partial-agg-move-param.sqlite");
    let source = temp.path().join("partial-agg-move-param.cdb");
    let ir_path = temp.path().join("take.ir.json");

    std::fs::write(
        &source,
        r#"
record Line { price_cents: i64 }

record Cursor<'a> { line: &'a mut Line, pos: i64 }

record Container<'a> { cursor: Cursor<'a>, tag: i64 }

fn take<'a>(container: Container<'a>) -> i64 =
  let c: Cursor<'a> = container.cursor in
  c.line.price_cents
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "take", "--out", path(&ir_path)]);
}

fn op_names(ir: &JsonValue) -> Vec<String> {
    ir["ir"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn asymmetric_conditional_move_through_case_is_accepted() {
    // Moving the move-only `ed` in only one `case` arm is now supported by
    // conditional drop glue (SPEC_V3 §7): lowering emits a compensating drop in
    // the arm that left `ed` live, so each arm drops it exactly once. Previously
    // rejected fail-closed.
    let temp = tempdir().unwrap();
    let db = temp.path().join("case-asymmetric-move.sqlite");
    let source = temp.path().join("case-asymmetric-move.cdb");
    let ir_path = temp.path().join("drop_in_one_arm.ir.json");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

enum Sel {
  yes: i64
  no: i64
}

fn consume<'a>(ed: LineEditor<'a>) -> i64 = ed.line.price_cents

fn drop_in_one_arm<'a>(sel: Sel, line: &'a mut Line) -> i64 =
  let ed: LineEditor<'a> = { line: line } in
  case sel of yes(u) => consume(ed) | no(v) => 0
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
    run(&["emit-ir", path(&db), "drop_in_one_arm", "--out", path(&ir_path)]);
}

#[test]
fn symmetric_conditional_move_through_case_is_accepted() {
    // Moving `ed` in *every* arm is a sound whole-slot move; the case move guard
    // must not over-reject it.
    let temp = tempdir().unwrap();
    let db = temp.path().join("case-symmetric-move.sqlite");
    let source = temp.path().join("case-symmetric-move.cdb");

    std::fs::write(
        &source,
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

enum Sel {
  yes: i64
  no: i64
}

fn consume<'a>(ed: LineEditor<'a>) -> i64 = ed.line.price_cents

fn drop_in_both_arms<'a>(sel: Sel, line: &'a mut Line) -> i64 =
  let ed: LineEditor<'a> = { line: line } in
  case sel of yes(u) => consume(ed) | no(v) => consume(ed)
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    run(&["verify", path(&db)]);
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
