// Canonical import order (SPEC_V3 §10/§11). The importer processes parsed items in a
// deterministic, source-order-independent canonical order (types before functions,
// each toposorted by dependencies with an alphabetical tie-break, mutually-recursive
// cliques as single units). So a program's migration sequence — and therefore every
// deterministic birth identity and the root hash — is a function of the item SET, not
// of how the source happens to be ordered. This makes import order-independent and
// import -> export -> import a fixpoint for ANY valid source ordering: the projection
// emits a canonical (name-sorted) order, and re-importing it reproduces the same root
// even though a hand-written source may be in a different order.

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

fn path(p: &Path) -> &str {
    p.to_str().expect("utf8 path")
}

fn root_hash(db: &Path) -> String {
    let history: JsonValue =
        serde_json::from_str(&run(&["history", path(db), "--json"])).expect("history json");
    history["root_hash"].as_str().expect("root_hash").to_string()
}

/// Import `source` into a fresh database and return its root hash.
fn import_root(name: &str, source: &str) -> String {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);
    root_hash(&db)
}

#[test]
fn import_root_is_independent_of_source_ordering() {
    // The same program written in two different (both valid) source orderings must
    // produce the same content-addressed root — the birth identities are a function of
    // the item set, not its position. Here `mmm` calls `aaa` and `zzz`, which are
    // otherwise independent: the projection would sort them `aaa, zzz`; this checks the
    // reverse hand-written order `zzz, aaa` reaches the identical root.
    // Identical bodies (so the only difference is declaration order); just the `aaa`
    // and `zzz` declarations are swapped.
    let ordering_a = "fn aaa() -> i64 = 1\nfn zzz() -> i64 = 2\nfn mmm() -> i64 = aaa() + zzz()\n";
    let ordering_b = "fn zzz() -> i64 = 2\nfn aaa() -> i64 = 1\nfn mmm() -> i64 = aaa() + zzz()\n";
    assert_eq!(
        import_root("order_a", ordering_a),
        import_root("order_b", ordering_b),
        "two source orderings of one program must reach the same root"
    );
}

#[test]
fn interleaved_types_and_functions_reach_a_canonical_root() {
    // Types and functions interleaved in two different orders (and the types in
    // non-alphabetical order) reach the same root: all types are created before any
    // function regardless of source position, and each kind is toposorted canonically.
    let ordering_a = "record Zed { v: i64 }\n\
                      fn use_zed(z: Zed) -> i64 = z.v\n\
                      record Abc { w: i64 }\n\
                      fn use_abc(a: Abc) -> i64 = a.w\n\
                      fn main() -> i64 = use_zed({ v: 5 }) + use_abc({ w: 3 })\n";
    let ordering_b = "record Abc { w: i64 }\n\
                      record Zed { v: i64 }\n\
                      fn main() -> i64 = use_zed({ v: 5 }) + use_abc({ w: 3 })\n\
                      fn use_abc(a: Abc) -> i64 = a.w\n\
                      fn use_zed(z: Zed) -> i64 = z.v\n";
    assert_eq!(
        import_root("inter_a", ordering_a),
        import_root("inter_b", ordering_b),
        "interleaved type/function orderings must reach the same root"
    );
}

#[test]
fn mutually_recursive_clique_root_is_order_independent() {
    // A mutually-recursive clique declared in two member orders reaches the same root
    // (the clique is one atomic unit with canonical member ordinals), and the
    // surrounding non-clique functions still canonicalize.
    let ordering_a = "fn even(n: i64) -> i64 = if n < 1 then 1 else odd(n - 1)\n\
                      fn odd(n: i64) -> i64 = if n < 1 then 0 else even(n - 1)\n\
                      fn main() -> i64 = even(10)\n";
    let ordering_b = "fn odd(n: i64) -> i64 = if n < 1 then 0 else even(n - 1)\n\
                      fn even(n: i64) -> i64 = if n < 1 then 1 else odd(n - 1)\n\
                      fn main() -> i64 = even(10)\n";
    assert_eq!(
        import_root("mut_a", ordering_a),
        import_root("mut_b", ordering_b),
        "clique member ordering must not change the root"
    );
}

#[test]
fn non_canonical_source_round_trips_to_a_fixpoint() {
    // The end-to-end guarantee (SPEC_V3 §11): a source NOT in canonical order still
    // round-trips import -> export -> import to a byte-stable projection AND an
    // identical root hash, because both imports canonicalize the order.
    let temp = tempdir().unwrap();
    let db = temp.path().join("rt.sqlite");
    let src = temp.path().join("rt.cdb");
    // Reverse-alphabetical, types after a function that doesn't use them, a clique.
    std::fs::write(
        &src,
        "fn zzz() -> i64 = 9\n\
         fn ping(n: i64) -> i64 = if n < 1 then 0 else pong(n - 1)\n\
         fn pong(n: i64) -> i64 = if n < 1 then 1 else ping(n - 1)\n\
         fn aaa() -> i64 = zzz()\n\
         fn main() -> i64 = aaa() + ping(7)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let export1 = temp.path().join("rt.export1.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export1)]);

    let db2 = temp.path().join("rt2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export1)]);
    run(&["verify", path(&db2)]);
    let export2 = temp.path().join("rt.export2.cdb");
    run(&["export", path(&db2), "--branch", "main", "--out", path(&export2)]);

    assert_eq!(
        std::fs::read_to_string(&export1).unwrap(),
        std::fs::read_to_string(&export2).unwrap(),
        "projection is byte-stable"
    );
    assert_eq!(
        root_hash(&db),
        root_hash(&db2),
        "import -> export -> import is a root-hash fixpoint for a non-canonical source"
    );
}
