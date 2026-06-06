//! Regression tests for loan-carrying records (SPEC_V2 §11/§12, PLAN_V2 Phase 8).
//!
//! These cover loan attribution and movement for records that hold references:
//! moving a loan-carrying record into another record literal, and reassigning a
//! reference field of such a record. They guard against an aliasing-`&mut`
//! soundness hole (reassigning one reference field silently dropping a sibling
//! field's loan) and against a false rejection of moving a move-only handle into
//! a new record value.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
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

fn import_source(name: &str, source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    (temp, db)
}

fn assert_rejected(name: &str, source: &str, expect: &[&str]) {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    let mut assertion = bin()
        .args(["import", path(&db), path(&src)])
        .assert()
        .failure();
    for needle in expect {
        assertion = assertion.stderr(predicate::str::contains(*needle));
    }
}

/// Moving a move-only (`&mut`-carrying) record into a new record literal must be
/// accepted: the binding's loan is transferred into the new value, not aliased.
/// The composed value then mutates through the moved handle and reads it back.
#[test]
fn moving_mutable_cursor_into_record_literal_is_accepted() {
    let (_temp, db) = import_source(
        "move-into-literal",
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

record Wrapper<'a> {
  ed: LineEditor<'a>
}

fn main<'a>() -> i64 effects[state] =
  let line: Line = { price_cents: 25, qty: 4 } in
  let editor: LineEditor<'a> = { line: &'a mut line } in
  let wrapped: Wrapper<'a> = { ed: editor } in
  let changed: unit = wrapped.ed.line.price_cents = wrapped.ed.line.price_cents + 75 in
  wrapped.ed.line.price_cents
"#,
    );
    assert_eq!(run(&["verify", path(&db)]).trim(), "verify ok");
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "100");
}

/// Using a move-only handle after it has been moved into a record literal must
/// be rejected as a use-after-move.
#[test]
fn use_after_move_into_record_literal_is_rejected() {
    assert_rejected(
        "uaf-into-literal",
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record LineEditor<'a> {
  line: &'a mut Line
}

record Wrapper<'a> {
  ed: LineEditor<'a>
}

fn main<'a>() -> i64 effects[state] =
  let line: Line = { price_cents: 25, qty: 4 } in
  let editor: LineEditor<'a> = { line: &'a mut line } in
  let wrapped: Wrapper<'a> = { ed: editor } in
  editor.line.price_cents
"#,
        &["bad_move", "use after move"],
    );
}

/// SOUNDNESS: a record produced by a call carries its loans at whole-record
/// granularity unless it has a single reference field. Reassigning one reference
/// field of a *two*-reference record must not silently end the sibling field's
/// loan — otherwise a second `&mut` to the sibling's referent would be wrongly
/// accepted, yielding two live `&mut` to the same place. This must fail closed.
#[test]
fn aliasing_mutable_refs_via_field_reassign_is_rejected() {
    assert_rejected(
        "aliasing-mut-reassign",
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record TwoRef<'a> {
  a: &'a mut Line
  b: &'a mut Line
}

fn make<'a>(x: &'a mut Line, y: &'a mut Line) -> TwoRef<'a> = { a: x, b: y }

fn main<'a>() -> i64 effects[state] =
  let lineA: Line = { price_cents: 1, qty: 1 } in
  let lineB: Line = { price_cents: 2, qty: 2 } in
  let lineC: Line = { price_cents: 3, qty: 3 } in
  let ed: TwoRef<'a> = make(&'a mut lineA, &'a mut lineB) in
  let rebind: unit = ed.a = &'a mut lineC in
  let probe: TwoRef<'a> = make(&'a mut lineB, &'a mut lineA) in
  ed.b.price_cents
"#,
        &["unsupported_assign"],
    );
}

/// A record with a *single* reference field built by a call carries that loan at
/// field granularity, so reassigning the field precisely ends the old loan and
/// the original referent may be borrowed again afterwards.
#[test]
fn single_reference_field_reassign_then_reborrow_is_accepted() {
    let (_temp, db) = import_source(
        "single-field-reassign",
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record Cursor<'a> {
  buf: &'a mut Line
  counter: i64
}

fn make<'a>(b: &'a mut Line) -> Cursor<'a> = { buf: b, counter: 0 }

fn main<'a>() -> i64 effects[state] =
  let line: Line = { price_cents: 25, qty: 4 } in
  let other: Line = { price_cents: 9, qty: 9 } in
  let ed: Cursor<'a> = make(&'a mut line) in
  let rebind: unit = ed.buf = &'a mut other in
  let ed2: Cursor<'a> = make(&'a mut line) in
  ed2.buf.price_cents
"#,
    );
    assert_eq!(run(&["verify", path(&db)]).trim(), "verify ok");
}

/// After reassigning the single reference field to a new referent, borrowing
/// that same new referent again while the field still holds it must conflict.
#[test]
fn reborrow_of_reassigned_target_is_rejected() {
    assert_rejected(
        "reassigned-target-conflict",
        r#"
record Line {
  price_cents: i64
  qty: i64
}

record Cursor<'a> {
  buf: &'a mut Line
  counter: i64
}

fn make<'a>(b: &'a mut Line) -> Cursor<'a> = { buf: b, counter: 0 }

fn main<'a>() -> i64 effects[state] =
  let line: Line = { price_cents: 25, qty: 4 } in
  let other: Line = { price_cents: 9, qty: 9 } in
  let ed: Cursor<'a> = make(&'a mut line) in
  let rebind: unit = ed.buf = &'a mut other in
  let ed2: Cursor<'a> = make(&'a mut other) in
  ed2.buf.price_cents
"#,
        &["exclusive loan conflict"],
    );
}
