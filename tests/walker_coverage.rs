// #12 — typed-DAG walker coverage. Six per-kind expression walkers (patch
// matching + reconstruction, bundle closure, blame-expr ancestry, break-expr
// reachability, verify hash-refs, member-rename descent) were hand-maintained
// and each drifted on a different subset of the V3.3 kinds: ANY patch failed
// root-wide if the root contained a `loop`/`return` function, and bundle
// export/import lost vec/string/loop children (`bad_bundle_closure`). They now
// derive from the central child table (`plain_child_expr_keys` /
// `for_each_child_expr_hash`) or the complete typed→raw converter, so these
// tests drive every rebuilt surface over one program that uses the newer kinds
// together: loop, early return, string builtins, array_fill, array_set, fold,
// and a guarded case.

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

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

const KIND_RICH_SOURCE: &str = "\
record Acc { hi: i64, lo: i64 }\n\
fn count_to(n: i64) -> i64 = loop acc = 0 while acc < n do acc + 1\n\
fn classify(b: i64) -> i64 = if b < 0 then return 0 else b + 1\n\
fn shout() -> i64 effects[alloc, state] =\n\
  let s: string = string_with_capacity(4) in\n\
  let pushed: unit = string_push(s, 33) in\n\
  string_len(s)\n\
fn fill_sum() -> i64 =\n\
  let a: array<i64, 4> = [7; 4] in\n\
  let b: array<i64, 4> = array_set(a, 2, 9) in\n\
  fold x in b with acc = 0 do acc + x\n\
fn pick(n: i64) -> i64 = case n of 0 => 10 | 1 if count_to(3) > 2 => 20 | _ => 30\n\
fn spread(a: Acc, n: i64) -> i64 = loop acc = 0 while acc < n do acc + a.hi + a.lo\n";

fn setup(db: &Path, src_dir: &Path) -> String {
    let src = src_dir.join("kinds.cdb");
    std::fs::write(&src, KIND_RICH_SOURCE).unwrap();
    run(&["init", path(db)]);
    run(&["import", path(db), path(&src)]);
    let history = parse_json(&run(&["history", path(db), "--json"]));
    history["root_hash"].as_str().expect("root_hash").to_string()
}

fn write_patch(dir: &Path, name: &str, value: JsonValue) -> std::path::PathBuf {
    let file = dir.join(name);
    std::fs::write(&file, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    file
}

#[test]
fn patching_a_literal_inside_a_loop_body_works() {
    // Before #12, ANY patch on this root failed with `unknown expression kind
    // loop` — first in match traversal, then in body reconstruction.
    let temp = tempdir().unwrap();
    let db = temp.path().join("patch-loop.sqlite");
    let root = setup(&db, temp.path());
    let patch = write_patch(
        temp.path(),
        "step.patch.json",
        serde_json::json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": { "kind": "literal_i64", "value": "1", "within_name": "count_to" },
            "replace": { "kind": "literal_i64", "value": "2" }
        }),
    );
    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "applied");
    // Step-2 loop: 0,2,4,6 — five is never hit, so count_to(5) is now 6.
    assert_eq!(run(&["eval", path(&db), "count_to", "5"]).trim(), "6");
    run(&["verify", path(&db)]);
}

#[test]
fn patching_a_return_operand_in_a_loop_bearing_root_works() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("patch-return.sqlite");
    let root = setup(&db, temp.path());
    let patch = write_patch(
        temp.path(),
        "ret.patch.json",
        serde_json::json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": { "kind": "literal_i64", "value": "0", "within_name": "classify" },
            "replace": { "kind": "literal_i64", "value": "99" }
        }),
    );
    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "applied");
    assert_eq!(run(&["eval", path(&db), "classify", "--", "-5"]).trim(), "99");
    run(&["verify", path(&db)]);
}

#[test]
fn bundle_round_trips_a_loop_string_array_program() {
    // Before #12 the bundle collector silently skipped loop/return/vec/string
    // children, so the exported closure was incomplete and import failed with
    // `bad_bundle_closure`.
    let temp = tempdir().unwrap();
    let db = temp.path().join("bundle-src.sqlite");
    let root = setup(&db, temp.path());
    let bundle = temp.path().join("kinds.bundle");
    run(&[
        "bundle",
        "export",
        path(&db),
        "--root",
        &root,
        "--out",
        path(&bundle),
    ]);
    let db2 = temp.path().join("bundle-dst.sqlite");
    run(&["init", path(&db2)]);
    run(&["bundle", "import", path(&db2), path(&bundle)]);
    assert_eq!(run(&["eval", path(&db2), "fill_sum"]).trim(), "30");
    assert_eq!(run(&["eval", path(&db2), "shout"]).trim(), "1");
    assert_eq!(run(&["eval", path(&db2), "pick", "1"]).trim(), "20");
    run(&["verify", path(&db2)]);
}

#[test]
fn blame_and_breakpoints_reach_inside_loop_and_string_bodies() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("blame.sqlite");
    setup(&db, temp.path());

    // blame-expr: the loop fn's body (and every node under it) has ancestry.
    let show = parse_json(&run(&["show", path(&db), "count_to", "--json"]));
    let body = show["body_hash"]
        .as_str()
        .unwrap_or_else(|| panic!("missing body hash: {show}"));
    let blame = parse_json(&run(&["blame-expr", path(&db), body, "--json"]));
    assert_eq!(blame["current_reachable"], true, "{blame}");

    // break-expr reachability now descends loop bodies: break on the loop
    // body expression itself and confirm the breakpoint is accepted.
    let report = {
        let mut cmd = bin();
        cmd.arg("debug")
            .arg(path(&db))
            .arg("count_to")
            .arg("2")
            .arg("--json")
            .arg("--cmd")
            .arg(format!("break expr {body}"))
            .arg("--cmd")
            .arg("continue");
        let output = cmd.assert().success().get_output().clone();
        parse_json(&String::from_utf8(output.stdout).expect("utf8 stdout"))
    };
    let commands = report["commands"].as_array().expect("commands");
    assert!(
        commands
            .iter()
            .any(|record| record["command"].as_str().is_some_and(|c| c.starts_with("break expr"))
                && record["error"].is_null()),
        "break expr must accept an expression inside the function: {report}"
    );
}

#[test]
fn member_rename_descends_loop_bodies() {
    // `spread` reads `a.hi` / `a.lo` inside a loop body. Before #12 the rename
    // rewriter did not descend `loop`, leaving the stale field name behind —
    // the migration then failed typecheck root-wide.
    let temp = tempdir().unwrap();
    let db = temp.path().join("rename.sqlite");
    let root = setup(&db, temp.path());
    let patch = write_patch(
        temp.path(),
        "rename.patch.json",
        serde_json::json!({
            "schema": "codedb/semantic-patch/v1",
            "branch": "main",
            "expected_root": root,
            "match": { "kind": "type", "name": "Acc" },
            "replace": { "kind": "rename_field", "field": "hi", "new_name": "high" }
        }),
    );
    let applied = parse_json(&run(&["patch", "apply", path(&db), "--json", path(&patch)]));
    assert_eq!(applied["status"], "applied", "{applied}");
    run(&["verify", path(&db)]);
    let export = temp.path().join("renamed.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let projected = std::fs::read_to_string(&export).unwrap();
    assert!(
        projected.contains("a.high") && !projected.contains("a.hi "),
        "the loop body's field access must be renamed: {projected}"
    );
}
