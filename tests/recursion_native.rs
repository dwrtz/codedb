// Phase 6 (R1) acceptance: recursion and mutual recursion compile to native
// artifacts and match the reference evaluator (the three-way oracle:
// eval == native). Covers self-recursion, multiple recursive calls in one body,
// mutual recursion (a cyclic call graph that `verify` must accept), and a
// recursive function over a recursive `box` heap type (recursion + recursive
// type layout + recursive drop glue). Recursion is created by the importer as a
// `CreateRecursionGroup` migration (SPEC_V3 §6) — members are bound before any
// body is type-checked — and projects back to ordinary `fn`s.

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

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

/// Import `source`, assert `entry` evaluates to `expected`, `verify`s, and (on a
/// buildable host) compiles to a native binary that returns the same value.
fn check_native(name: &str, source: &str, entry: &str, expected: i64) {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    assert_eq!(
        run(&["eval", path(&db), entry]).trim(),
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
            entry,
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
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": expected.to_string()}),
            "{name}: native actual value"
        );
    }
}

#[test]
fn self_recursive_factorial_compiles_native_and_matches_oracle() {
    check_native(
        "factorial",
        "fn fact(n: i64) -> i64 = if n < 1 then 1 else n * fact(n - 1)\n\
         fn main() -> i64 = fact(5)\n",
        "main",
        120,
    );
}

#[test]
fn self_recursive_fibonacci_with_two_recursive_calls_matches_oracle() {
    // Two recursive calls in one body — the recursion group still resolves both
    // self-references, and both lower to native calls.
    check_native(
        "fibonacci",
        "fn fib(n: i64) -> i64 = if n < 2 then n else fib(n - 1) + fib(n - 2)\n\
         fn main() -> i64 = fib(10)\n",
        "main",
        55,
    );
}

#[test]
fn mutual_recursion_compiles_native_and_verify_accepts_the_cycle() {
    // is_even/is_odd form a 2-cycle in the call graph; `verify` must accept the
    // cyclic graph (effects, borrows) and both compile to native.
    check_native(
        "even_odd",
        "fn is_even(n: i64) -> i64 = if n < 1 then 1 else is_odd(n - 1)\n\
         fn is_odd(n: i64) -> i64 = if n < 1 then 0 else is_even(n - 1)\n\
         fn main() -> i64 = is_even(10) * 10 + is_odd(10)\n",
        "main",
        10,
    );
}

#[test]
fn recursion_over_recursive_box_heap_type_builds_and_drops_natively() {
    // `build` is self-recursive AND returns a recursive `box` heap type. This
    // exercises recursion + recursive type layout + recursive drop glue: a chain
    // of `n` heap nodes is allocated by recursive descent and freed when `tree`
    // leaves scope (a double-free would abort the native run). Regression guard
    // for the recursive-type cycle in reference-region collection.
    check_native(
        "box_recursion",
        "enum Node { empty: unit  next: box<Node> }\n\
         fn build(n: i64) -> Node effects[alloc] =\n\
           if n < 1 then Node::empty(()) else Node::next(box_new(build(n - 1)))\n\
         fn main() -> i64 effects[alloc] = let tree: Node = build(6) in 42\n",
        "main",
        42,
    );
}

#[test]
fn unbox_moves_an_owned_box_payload_out_and_frees_the_shell() {
    // `unbox(b: box<T>) -> T` (Phase 6 deref-by-move): moves the `Cons` record out
    // of the heap into an owned local and frees the box shell. A double-free aborts
    // the native run; the evaluator agrees on the value.
    check_native(
        "unbox_local",
        "record Cons { head: i64\n  tail: i64 }\n\
         fn f() -> i64 effects[alloc] =\n\
           let b: box<Cons> = box_new({ head: 7, tail: 3 }) in\n\
           let c: Cons = unbox(b) in c.head + c.tail\n\
         fn main() -> i64 effects[alloc] = f()\n",
        "main",
        10,
    );
}

#[test]
fn unbox_of_a_scalar_box_loads_the_value_and_frees() {
    // The scalar `unbox` path (box<i64> -> i64): the value is loaded straight into
    // the result and the box is freed (the `emit_unbox_move` scalar branch).
    check_native(
        "unbox_scalar",
        "fn f() -> i64 effects[alloc] =\n\
           let b: box<i64> = box_new(41) in unbox(b) + 1\n\
         fn main() -> i64 effects[alloc] = f()\n",
        "main",
        42,
    );
}

#[test]
fn unbox_transfers_nested_box_ownership() {
    // box<Outer> where Outer owns box<Inner>: unbox frees the outer shell and
    // transfers ownership of the inner box to the result; a second unbox frees the
    // inner box. Each block is freed exactly once (a mishandled free aborts).
    check_native(
        "unbox_nested",
        "record Inner { x: i64 }\n\
         record Outer { inner: box<Inner>\n  y: i64 }\n\
         fn f() -> i64 effects[alloc] =\n\
           let b: box<Outer> = box_new({ inner: box_new({ x: 5 }), y: 2 }) in\n\
           let o: Outer = unbox(b) in\n\
           let inner: Inner = unbox(o.inner) in inner.x + o.y\n\
         fn main() -> i64 effects[alloc] = f()\n",
        "main",
        7,
    );
}

#[test]
fn case_traversal_of_a_recursive_box_heap_lengths_and_frees_natively() {
    // The Phase 6 acceptance fixture (formerly Deferred): traverse a recursive
    // `box<Node>` heap by `case`. The `next` arm binds the move-only box payload and
    // `unbox`es it to recurse; the scrutinee is consumed, so each of the 6 allocated
    // nodes is freed exactly once (a double-free aborts; the evaluator agrees).
    check_native(
        "node_length",
        "enum Node { empty: unit\n  next: box<Node> }\n\
         fn build(n: i64) -> Node effects[alloc] =\n\
           if n < 1 then Node::empty(()) else Node::next(box_new(build(n - 1)))\n\
         fn length(n: Node) -> i64 effects[alloc] =\n\
           case n of\n\
             empty(u) => 0\n\
           | next(boxed) => 1 + length(unbox(boxed))\n\
         fn main() -> i64 effects[alloc] = length(build(6))\n",
        "main",
        6,
    );
}

#[test]
fn case_arm_that_ignores_a_box_binding_drops_it_exactly_once() {
    // The `next` arm binds the box payload but does NOT consume it: the binding's
    // scope-exit residual drop must free the bound box (and recursively its
    // sub-chain) exactly once, while the consumed scrutinee is never dropped.
    check_native(
        "node_ignored_binding",
        "enum Node { empty: unit\n  next: box<Node> }\n\
         fn build(n: i64) -> Node effects[alloc] =\n\
           if n < 1 then Node::empty(()) else Node::next(box_new(build(n - 1)))\n\
         fn first(n: Node) -> i64 effects[alloc] =\n\
           case n of empty(u) => 0 | next(boxed) => 99\n\
         fn main() -> i64 effects[alloc] = first(build(4))\n",
        "main",
        99,
    );
}

/// Collect op kinds recursively, descending into `if`/`case` blocks — a default
/// arm's drop lives inside the arm block, not at the top level.
fn op_kinds_recursive(ops: &JsonValue, out: &mut Vec<String>) {
    for op in ops.as_array().unwrap() {
        out.push(op["op"].as_str().unwrap().to_string());
        for key in ["then_block", "else_block"] {
            if let Some(block) = op.get(key) {
                op_kinds_recursive(&block["operations"], out);
            }
        }
        if let Some(arms) = op.get("arms").and_then(JsonValue::as_array) {
            for arm in arms {
                op_kinds_recursive(&arm["block"]["operations"], out);
            }
        }
    }
}

const DEFAULT_ARM_SOURCE: &str =
    "enum Node { empty: unit\n  next: box<Node> }\n\
     fn build(n: i64) -> Node effects[alloc] =\n\
       if n < 1 then Node::empty(()) else Node::next(box_new(build(n - 1)))\n\
     fn classify(n: Node) -> i64 effects[alloc] =\n\
       case n of empty(u) => 0 | _ => 42\n\
     fn main() -> i64 effects[alloc] = classify(build(5))\n";

#[test]
fn default_case_arm_over_a_box_variant_frees_its_payload() {
    // Regression (SPEC_V3 §7 exactly-once): a `case` whose `_`/default arm matches a
    // move-only `box`-carrying variant must FREE that payload. The move-only enum
    // scrutinee is consumed (the param place is `Move`d), so nothing else drops it —
    // before the fix the default arm emitted ZERO drops and the whole `box<Node>`
    // sub-chain leaked, yet eval/verify/native all "passed" (a leak changes neither
    // the value nor aborts the run, and `verify` checks only at-most-once). Pin the
    // fix by asserting the lowered default arm now drops the payload.
    let temp = tempdir().unwrap();
    let db = temp.path().join("defaultarm.sqlite");
    let src = temp.path().join("defaultarm.cdb");
    std::fs::write(&src, DEFAULT_ARM_SOURCE).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);

    let ir_path = temp.path().join("classify.ir.json");
    run(&["emit-ir", path(&db), "classify", "--out", path(&ir_path)]);
    let ir: JsonValue =
        serde_json::from_str(&std::fs::read_to_string(&ir_path).unwrap()).unwrap();
    let mut ops = Vec::new();
    op_kinds_recursive(&ir["ir"]["operations"], &mut ops);
    assert!(
        ops.iter().any(|op| op == "drop"),
        "the `_` arm must free the matched box<Node> payload (leak guard), got {ops:?}"
    );

    // eval + native: the value is correct and the added drop does not double-free
    // (a double-free would abort the native run).
    check_native("defaultarm", DEFAULT_ARM_SOURCE, "main", 42);
}

#[test]
fn recursion_members_project_back_to_plain_functions() {
    // A recursion group is an internal representation: members export as ordinary
    // `fn`s (no special syntax), so the checked-view projection round-trips.
    let temp = tempdir().unwrap();
    let db = temp.path().join("project.sqlite");
    let src = temp.path().join("project.cdb");
    let export = temp.path().join("project.export.cdb");
    std::fs::write(
        &src,
        "fn ping(n: i64) -> i64 = if n < 1 then 0 else pong(n - 1)\n\
         fn pong(n: i64) -> i64 = if n < 1 then 1 else ping(n - 1)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["export", path(&db), "--branch", "main", "--out", path(&export)]);
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(exported.contains("fn ping(n: i64) -> i64"), "ping projects: {exported}");
    assert!(exported.contains("fn pong(n: i64) -> i64"), "pong projects: {exported}");
    // Re-importing the projection succeeds (the clique is re-detected).
    let db2 = temp.path().join("project2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export)]);
    run(&["verify", path(&db2)]);
}

#[test]
fn deep_recursion_evaluator_ceiling_is_an_oracle_bound_not_a_native_limit() {
    // The reference evaluator walks the HOST stack, so a deep / non-terminating
    // program would overflow it and abort the process (SIGABRT) instead of returning
    // an error. A call-recursion ceiling now converts that into a clean, recoverable
    // error. Crucially it is an ORACLE bound, not a language limit: the native backend
    // runs on the OS stack and compiles + runs the SAME program. A depth well past the
    // evaluator ceiling (debug 120 / release 1000) but trivial for the OS stack
    // exercises both halves.
    let temp = tempdir().unwrap();
    let db = temp.path().join("deep.sqlite");
    let src = temp.path().join("deep.cdb");
    std::fs::write(
        &src,
        "fn deep(n: i64) -> i64 = if n < 1 then 0 else 1 + deep(n - 1)\n\
         fn main() -> i64 = deep(2000)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    // Evaluator: a clean ceiling error, NOT a stack-overflow abort.
    let output = bin()
        .args(["eval", path(&db), "main"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("call-recursion ceiling"),
        "expected a clean evaluator ceiling error, got: {stderr}"
    );

    // Native: the same program compiles and runs (deep(2000) = 2000; the executable's
    // exit status is the result truncated to a byte, 2000 & 0xff = 208).
    if can_build_default_native_target() {
        let exe = temp.path().join("deep.exe");
        run(&["build", path(&db), "main", "--out", path(&exe)]);
        let status = StdCommand::new(&exe).status().unwrap();
        assert_eq!(
            status.code(),
            Some(2000 & 0xff),
            "native deep recursion should run on the OS stack unaffected by the oracle ceiling"
        );
    }
}

#[test]
fn inline_move_only_enum_payload_moves_out_of_a_case_arm_native() {
    // V3.1 fail-closed item #3 lifted: an INLINE (non-box) move-only aggregate payload
    // — a record owning a box — can now be bound and moved out of a `case` arm when the
    // scrutinee is a consumed place. Lowering reads it out with a `Load`-aliased pointer
    // + `Store` memcpy (a shallow byte move); the consumed enum is never dropped, so the
    // box is freed exactly once (by `consume` via `unbox`). eval == native; a double-free
    // would abort the run. (Leak-freedom at scale is pinned by tests/leak_interposer.rs.)
    check_native(
        "inline_enum_payload",
        "record Boxed { b: box<i64> }\n\
         enum E { only: Boxed }\n\
         fn consume(x: Boxed) -> i64 effects[alloc] = unbox(x.b)\n\
         fn f(e: E) -> i64 effects[alloc] = case e of only(x) => consume(x)\n\
         fn main() -> i64 effects[alloc] = f(E::only({ b: box_new(42) }))\n",
        "main",
        42,
    );
}

#[test]
fn moving_inline_enum_payload_from_a_temporary_scrutinee_fails_closed() {
    // The remaining restriction: a non-place (temporary) move-only enum scrutinee is
    // not drop-tracked, so moving its payload out stays fail-closed with a clean
    // diagnostic (the checker accepts it; lowering declines). No crash.
    let temp = tempdir().unwrap();
    let db = temp.path().join("temp_scrut.sqlite");
    let src = temp.path().join("temp_scrut.cdb");
    std::fs::write(
        &src,
        "record Boxed { b: box<i64> }\n\
         enum E { only: Boxed }\n\
         fn consume(x: Boxed) -> i64 effects[alloc] = unbox(x.b)\n\
         fn g() -> i64 effects[alloc] = case E::only({ b: box_new(1) }) of only(x) => consume(x)\n\
         fn main() -> i64 effects[alloc] = g()\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let ir = temp.path().join("g.ir.json");
    let stderr = String::from_utf8(
        bin()
            .args(["emit-ir", path(&db), "g", "--out", path(&ir)])
            .assert()
            .failure()
            .get_output()
            .stderr
            .clone(),
    )
    .unwrap();
    assert!(
        stderr.contains("consumed (param/local) scrutinee"),
        "expected a clean fail-closed diagnostic, got: {stderr}"
    );
}
