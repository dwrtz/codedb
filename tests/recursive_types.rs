// Mutually-recursive TYPE definitions (SPEC_V3 §6, D1): a `record`/`enum` clique
// that references itself across the cycle (e.g. `Cons` <-> `List`) is created
// atomically by a `CreateTypeGroup` migration — every member's name is bound before
// any definition is resolved, so cross-references resolve. The clique's content
// identity is canonical (source-order-independent, fixpoint round-trip), and the
// `box` indirection that breaks the size cycle lets these heaps be built, traversed
// by `case` + `unbox`, and freed exactly once natively.

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

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

fn root_hash(db: &Path) -> String {
    let history = parse_json(&run(&["history", path(db), "--json"]));
    history["root_hash"].as_str().expect("root_hash").to_string()
}

/// Import `source`, assert `entry` evaluates to `expected`, `verify`s, and (on a
/// buildable host) compiles to a native binary returning the same value — a
/// double-free in the recursive drop glue aborts the run, so a passing native run
/// confirms every heap node was freed exactly once.
fn check_native(name: &str, source: &str, expected: i64) {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    assert_eq!(
        run(&["eval", path(&db), "main"]).trim(),
        expected.to_string(),
        "{name}: evaluator result"
    );
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            &format!("{name}_native"),
            "--entry",
            "main",
            "--expect-i64",
            &expected.to_string(),
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied", "{name}: create-test");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed", "{name}: native test status");
        assert_eq!(report["native_mismatches"], 0, "{name}: native mismatches");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"]["value"],
            expected.to_string(),
            "{name}: native value"
        );
    }
}

const CONS_LIST: &str = r#"
record Cons { head: i64
  tail: List }
enum List { nil: unit
  cons: box<Cons> }

fn sum(l: List) -> i64 effects[alloc] =
  case l of
    nil(u) => 0
  | cons(boxed) => let c: Cons = unbox(boxed) in c.head + sum(c.tail)

fn build() -> List effects[alloc] =
  List::cons(box_new({ head: 10, tail: List::cons(box_new({ head: 20, tail: List::nil(()) })) }))

fn main() -> i64 effects[alloc] = sum(build())
"#;

#[test]
fn mutually_recursive_cons_list_sums_and_frees_natively() {
    // Cons <-> List: per-node data with a recursive tail. Traversed by case + unbox;
    // 2 nodes allocated, each freed exactly once. The record literal under
    // `box_new` is coerced to the nominal `Cons` (expected-type propagation).
    check_native("cons_list_sum", CONS_LIST, 30);
}

#[test]
fn mutually_recursive_tree_walking_evaluator_compiles_native() {
    // Expr <-> Pair: a tree-walking expression evaluator (the Phase 6 named
    // fixture). (2 + 3) * 4 = 20; every heap Pair is freed exactly once.
    check_native(
        "tree_walker",
        r#"
record Pair { left: Expr
  right: Expr }
enum Expr { lit: i64
  add: box<Pair>
  mul: box<Pair> }

fn eval(e: Expr) -> i64 effects[alloc] =
  case e of
    lit(v) => v
  | add(boxed) => let p: Pair = unbox(boxed) in eval(p.left) + eval(p.right)
  | mul(boxed) => let p: Pair = unbox(boxed) in eval(p.left) * eval(p.right)

fn main() -> i64 effects[alloc] =
  eval(Expr::mul(box_new({
    left: Expr::add(box_new({ left: Expr::lit(2), right: Expr::lit(3) })),
    right: Expr::lit(4)
  })))
"#,
        20,
    );
}

#[test]
fn type_clique_hash_is_source_order_independent() {
    // Declaring the clique record-first vs enum-first yields the SAME root hash:
    // member ordinals derive from the clique's structure (canonical labeling), not
    // source declaration order (SPEC_V3 §6/§10).
    let cons_first = "record Cons { head: i64\n  tail: List }\n\
                      enum List { nil: unit\n  cons: box<Cons> }\n";
    let list_first = "enum List { nil: unit\n  cons: box<Cons> }\n\
                      record Cons { head: i64\n  tail: List }\n";
    let temp = tempdir().unwrap();
    let db1 = temp.path().join("cons_first.sqlite");
    let db2 = temp.path().join("list_first.sqlite");
    for (db, src) in [(&db1, cons_first), (&db2, list_first)] {
        let file = temp.path().join(format!("{}.cdb", path(db).len()));
        std::fs::write(&file, src).unwrap();
        run(&["init", path(db)]);
        run(&["import", path(db), path(&file)]);
    }
    assert_eq!(
        root_hash(&db1),
        root_hash(&db2),
        "mutually-recursive type clique hash must be source-order-independent"
    );
}

#[test]
fn automorphic_type_clique_hash_is_source_order_independent() {
    // Regression (SPEC_V3 §6/§10): a mutually-recursive *record* clique with a
    // structural automorphism but DISTINCT field names — `A.toB` <-> `B.toA` — must
    // hash the same regardless of source declaration order. Both members have an
    // identical recolored type (`{i64, box<peer>}`), so the canonical labeling can
    // only tell them apart by field NAME. Before the fix the name was erased from the
    // clique-ordering form, the two members formed one unsplittable orbit, the
    // member→ordinal mapping fell to source order, and — because the names differ in
    // the final `TypeDef` identity — the two orderings produced DIFFERENT root hashes
    // and a non-fixpoint round-trip. (Cons/List can't exercise this: a record and an
    // enum are discretized by kind, so no automorphism arises.)
    let a_first = "record A { v: i64\n  toB: box<B> }\n\
                   record B { v: i64\n  toA: box<A> }\n\
                   fn main() -> i64 = 0\n";
    let b_first = "record B { v: i64\n  toA: box<A> }\n\
                   record A { v: i64\n  toB: box<B> }\n\
                   fn main() -> i64 = 0\n";
    let temp = tempdir().unwrap();
    let db1 = temp.path().join("a_first.sqlite");
    let db2 = temp.path().join("b_first.sqlite");
    for (db, src) in [(&db1, a_first), (&db2, b_first)] {
        let file = temp.path().join(format!("{}.cdb", path(db).len()));
        std::fs::write(&file, src).unwrap();
        run(&["init", path(db)]);
        run(&["import", path(db), path(&file)]);
    }
    assert_eq!(
        root_hash(&db1),
        root_hash(&db2),
        "automorphic record clique hash must be source-order-independent"
    );

    // import -> export -> re-import is a fixpoint (the SPEC_V3 §11 round-trip gate).
    let export = temp.path().join("auto.export.cdb");
    run(&["export", path(&db1), "--branch", "main", "--out", path(&export)]);
    let db3 = temp.path().join("auto.rt.sqlite");
    run(&["init", path(&db3)]);
    run(&["import", path(&db3), path(&export)]);
    assert_eq!(
        root_hash(&db1),
        root_hash(&db3),
        "automorphic record clique must round-trip through the projection"
    );

    // `verify` recomputes each type clique's canonical member ordinals from the
    // re-projected source (SPEC_V3 §10) and rejects a permutation. A->B / B->A is the
    // hard case (a structural automorphism distinguished only by field NAME), so the
    // recompute must run individualization-refinement and still reproduce the minted
    // ordinals — it must NOT false-reject either source ordering or the round-trip.
    for db in [&db1, &db2, &db3] {
        bin().args(["verify", path(db)]).assert().success().stdout("verify ok\n");
    }
}

#[test]
fn type_clique_projection_round_trip_is_a_fixpoint() {
    // import -> export -> re-import reproduces the SAME root hash: clique members
    // project back as plain `record`/`enum`s (no identity pins — re-derived
    // canonically, like function recursion-group members), so the checked-view
    // round-trip is identity-preserving (SPEC_V3 §11). The earlier attempt pinned
    // clique identities, which changed the re-imported op and shifted every
    // downstream symbol's birth. A single `main` is used so the fixpoint isolates
    // the type clique (multiple non-clique helpers would re-expose the pre-existing
    // idx-based birth-seed sensitivity to export function reordering).
    let source = "record Cons { head: i64\n  tail: List }\n\
                  enum List { nil: unit\n  cons: box<Cons> }\n\
                  fn main() -> i64 effects[alloc] =\n\
                    let l: List = List::cons(box_new({ head: 7, tail: List::nil(()) })) in\n\
                    case l of nil(u) => 0 | cons(b) => let c: Cons = unbox(b) in c.head\n";
    let temp = tempdir().unwrap();
    let db = temp.path().join("rt.sqlite");
    let src = temp.path().join("rt.cdb");
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let root1 = root_hash(&db);

    let export = temp.path().join("rt.export.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let db2 = temp.path().join("rt2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export)]);

    assert_eq!(
        root1,
        root_hash(&db2),
        "mutually-recursive type clique must round-trip through the projection"
    );
    assert_eq!(run(&["eval", path(&db2), "main"]).trim(), "7");
}

#[test]
fn type_clique_members_project_back_to_plain_type_definitions() {
    // A type recursion group is an internal representation: members export as
    // ordinary `record`/`enum`s (no special syntax), so the checked view re-parses.
    let temp = tempdir().unwrap();
    let db = temp.path().join("project.sqlite");
    let src = temp.path().join("project.cdb");
    let export = temp.path().join("project.export.cdb");
    std::fs::write(&src, CONS_LIST).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(exported.contains("record Cons"), "Cons projects: {exported}");
    assert!(exported.contains("enum List"), "List projects: {exported}");
    let db2 = temp.path().join("project2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export)]);
    run(&["verify", path(&db2)]);
}
