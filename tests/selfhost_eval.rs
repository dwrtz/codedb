// Phase 8 (ladder rung 0) Stage 2: the CodeDB-hosted evaluator's scalar core.
//
// `compiler/eval/eval.cdb` is imported, verified, built to a NATIVE binary,
// and fed real `emit-cir` artifacts on stdin. It must EXECUTE the entry and
// print `ok:<value>` (exit 0) or `trap:<code>` (exit 101), and its results
// must equal the Rust reference evaluator's on the same programs — the
// rung-0 result-equality oracle (SPEC_V3 §5), with the native backend as the
// transitive third leg via the existing per-feature eval==native suites.
//
// Coverage here: scalar control flow (if, early return, calls, recursion),
// every registry operator kind at its own width/signedness (the generated
// three-way sweep below asserts a fixture per `codedb::operator_kinds()`
// entry), int casts, div/mod trap parity, and the fail-closed shell.
// Aggregates/case/fold/loop/heap are pinned to `trap:unsupported_op` until
// Stages 3/4 flip them.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::OnceLock;

use assert_cmd::Command;
use tempfile::{tempdir, TempDir};

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

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

/// Import the committed evaluator sources, verify, and build the native
/// evaluator binary — once per test process (the tests share the artifact).
fn evaluator() -> &'static Path {
    static EVALUATOR: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    EVALUATOR
        .get_or_init(|| {
            let temp = tempdir().unwrap();
            let db = temp.path().join("selfhost-eval.sqlite");
            run(&["init", path(&db)]);
            run(&["import", path(&db), "std/fmt.cdb"]);
            run(&["import", path(&db), "compiler/eval/eval.cdb"]);
            run(&["verify", path(&db)]);
            let exe = temp.path().join("eval-bin");
            run(&["build", path(&db), "main", "--out", path(&exe)]);
            (temp, exe)
        })
        .1
        .as_path()
}

/// Init + import a fixture source file; return the db path.
fn import_fixture(temp: &Path, name: &str, source: &str) -> PathBuf {
    let db = temp.join(format!("{name}.sqlite"));
    let src = temp.join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    db
}

/// Emit the CIR for `entry` of `db` into `<entry>.cir` under temp.
fn emit_cir(temp: &Path, db: &Path, entry: &str) -> PathBuf {
    let cir = temp.join(format!("{entry}.cir"));
    run(&["emit-cir", path(db), entry, "--out", path(&cir)]);
    cir
}

/// Run the native evaluator with `input` on stdin; return (exit, stdout).
fn run_evaluator(exe: &Path, input: &Path) -> (i32, String) {
    let output = StdCommand::new(exe)
        .stdin(Stdio::from(File::open(input).expect("open evaluator input")))
        .output()
        .expect("run evaluator binary");
    (
        output.status.code().expect("evaluator exit code"),
        String::from_utf8(output.stdout).expect("utf8 evaluator stdout"),
    )
}

/// Map the Rust evaluator's printed value onto the .cdb evaluator's output
/// protocol: bool as 0/1, unit as `unit`, numbers as-is.
fn normalize_rust_eval(out: &str) -> String {
    match out.trim() {
        "true" => "1".to_string(),
        "false" => "0".to_string(),
        "()" => "unit".to_string(),
        other => other.to_string(),
    }
}

/// Assert that the .cdb evaluator's result equals the Rust evaluator's for
/// one entry of one fixture db.
fn assert_three_way(temp: &Path, exe: &Path, db: &Path, entry: &str) {
    let cir = emit_cir(temp, db, entry);
    let rust = normalize_rust_eval(&run(&["eval", path(db), entry]));
    let (code, stdout) = run_evaluator(exe, &cir);
    assert_eq!(
        (code, stdout.trim()),
        (0, format!("ok:{rust}").as_str()),
        "{entry}: CodeDB-eval vs Rust-eval"
    );
}

const SCALAR_FIXTURE: &str = "\
fn pick(n: i64) -> i64 = if n > 10 then n * 2 else n - 1\n\
fn guard(n: i64) -> i64 =\n\
  let d: i64 = (if n == 0 then return 0 - 99 else 1000 / n) in\n\
  d + 1\n\
fn wrap32(a: u32, b: u32) -> u32 = a + b\n\
fn sgn8(x: i64) -> i64 = to_i64(to_i8(x))\n\
fn fib(n: i64) -> i64 = if n < 2 then n else fib(n - 1) + fib(n - 2)\n\
fn main() -> i64 = pick(7) + pick(20) + guard(8) + guard(0) + to_i64(wrap32(0xffffffff, 2)) + sgn8(255) + fib(15)\n\
fn t_bool() -> bool = 3 < 5 && !(2 == 2) || 7 >= 7\n\
fn t_unit() -> unit = ()\n\
fn t_u64() -> u64 = 0xffffffffffffffff - 4\n\
fn t_u32() -> u32 = wrap32(0xfffffff0, 0x20)\n\
fn t_div0() -> i64 = 7 / (3 - 3)\n\
fn t_mod0() -> i64 = 7 % (3 - 3)\n\
fn t_shift() -> i64 = to_i64(to_u8(1) << to_u8(9)) + (0 - 8 >> 1) + to_i64(to_u32(0x80000000) >> to_u32(4))\n\
fn t_neg() -> i64 = to_i64(~to_u16(0)) + to_i64(to_i32(0) - to_i32(0x80000000))\n";

#[test]
fn scalar_programs_match_the_rust_evaluator() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let exe = evaluator();
    let db = import_fixture(temp.path(), "scalar", SCALAR_FIXTURE);

    // Control flow, early return, recursion, casts, widths, rendering.
    for entry in [
        "main", "t_bool", "t_unit", "t_u64", "t_u32", "t_shift", "t_neg",
    ] {
        assert_three_way(temp.path(), exe, &db, entry);
    }

    // Trap parity: the Rust evaluator errors; the .cdb evaluator prints the
    // trap code and exits 101.
    for (entry, code_name) in [("t_div0", "division_by_zero"), ("t_mod0", "modulo_by_zero")] {
        let cir = emit_cir(temp.path(), &db, entry);
        let rust_err = run_fail(&["eval", path(&db), entry]);
        assert!(
            rust_err.contains("by zero"),
            "{entry}: rust eval should trap: {rust_err}"
        );
        let (code, stdout) = run_evaluator(exe, &cir);
        assert_eq!(
            (code, stdout.trim()),
            (101, format!("trap:{code_name}").as_str()),
            "{entry}: trap parity"
        );
    }
}

/// One generated fixture per registry operator kind, at that kind's own
/// width and signedness, with operands that exercise wrap/sign edges (an
/// unsigned high-bit left operand discriminates unsigned from signed
/// compares; an over-MAX sum discriminates wrapping from promotion).
fn operator_fixture(kind: &str) -> (String, String) {
    let name = format!("k_{kind}");
    if let Some(body) = match kind {
        "and_bool" => Some("true && (1 == 2)".to_string()),
        "or_bool" => Some("(1 == 2) || true".to_string()),
        "not_bool" => Some("!(3 == 3)".to_string()),
        _ => None,
    } {
        return (name.clone(), format!("fn {name}() -> bool = {body}\n"));
    }
    let (verb, ty) = kind.rsplit_once('_').expect("kind has a width suffix");
    let (a, b) = match ty {
        "i8" => ("0x75", "0x2c"),
        "i16" => ("0x7ff5", "0x2d"),
        "i32" => ("0x7ffffff5", "0x3d"),
        "i64" => ("0x7ffffffffffffff5", "0x4d"),
        "u8" => ("0xf3", "0x1d"),
        "u16" => ("0xfff3", "0x2d"),
        "u32" => ("0xfffffff3", "0x3d"),
        "u64" => ("0xfffffffffffffff3", "0x4d"),
        other => panic!("unknown operator width {other}"),
    };
    let cast = |v: &str| format!("to_{ty}({v})");
    let (expr, ret) = match verb {
        "add" => (format!("{} + {}", cast(a), cast(b)), ty),
        "sub" => (format!("{} - {}", cast(b), cast(a)), ty),
        "mul" => (format!("{} * {}", cast(a), cast(b)), ty),
        "div" => (format!("{} / {}", cast(a), cast(b)), ty),
        "mod" => (format!("{} % {}", cast(a), cast(b)), ty),
        "and" => (format!("{} & {}", cast(a), cast(b)), ty),
        "or" => (format!("({} | {})", cast(a), cast(b)), ty),
        "xor" => (format!("{} ^ {}", cast(a), cast(b)), ty),
        "shl" => (format!("{} << {}", cast(a), cast("3")), ty),
        "shr" => (format!("{} >> {}", cast(a), cast("3")), ty),
        "eq" => (format!("{} == {}", cast(a), cast(b)), "bool"),
        "ne" => (format!("{} != {}", cast(a), cast(b)), "bool"),
        "lt" => (format!("{} < {}", cast(a), cast(b)), "bool"),
        "le" => (format!("{} <= {}", cast(a), cast(b)), "bool"),
        "gt" => (format!("{} > {}", cast(a), cast(b)), "bool"),
        "ge" => (format!("{} >= {}", cast(a), cast(b)), "bool"),
        "neg" => (format!("-{}", cast(a)), ty),
        "bitnot" => (format!("~{}", cast(a)), ty),
        other => panic!("unknown operator verb {other}"),
    };
    (name.clone(), format!("fn {name}() -> {ret} = {expr}\n"))
}

#[test]
fn every_operator_kind_agrees_three_way() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let exe = evaluator();

    let kinds = codedb::operator_kinds();
    let mut source = String::new();
    let mut entries = Vec::new();
    for kind in &kinds {
        let (name, fn_src) = operator_fixture(kind);
        source.push_str(&fn_src);
        entries.push(name);
    }
    // The coverage gate: a registry kind without a fixture fails loudly.
    assert_eq!(entries.len(), kinds.len(), "fixture per operator kind");

    let db = import_fixture(temp.path(), "conformance", &source);
    for entry in &entries {
        assert_three_way(temp.path(), exe, &db, entry);
    }
}

const AGGREGATE_FIXTURE: &str = "\
record P {\n  x: i64\n  y: u32\n}\n\
record Outer {\n  p: P\n  tag: i64\n}\n\
enum Shape {\n  circle: i64\n  square: P\n  empty: unit\n}\n\
fn t_record() -> i64 =\n\
  let o: Outer = { p: { x: 40, y: 0x10 }, tag: 7 } in\n\
  o.p.x + to_i64(o.p.y) + o.tag\n\
fn mk(n: i64) -> Shape =\n\
  if n > 0 then Shape::circle(n)\n\
  else if n == 0 then Shape::empty(())\n\
  else Shape::square({ x: n, y: 3 })\n\
fn classify(s: Shape) -> i64 =\n\
  case s of circle(r) => r * 2 | square(p) => p.x | empty(u) => 0 - 5\n\
fn t_enum() -> i64 = classify(mk(21)) + classify(mk(0)) + classify(mk(0 - 9))\n\
fn t_array() -> i64 =\n\
  let xs: array<i64, 5> = [10, 20, 30, 40, 50] in\n\
  let i: i64 = 3 in\n\
  xs[i] + xs[0]\n\
fn t_fill_set() -> i64 =\n\
  let xs: array<u32, 8> = array_set([0x5; 8], 2, 0xff) in\n\
  to_i64(xs[2]) + to_i64(xs[7])\n\
fn t_fold() -> i64 =\n\
  let xs: array<i64, 4> = [2, 4, 6, 8] in\n\
  fold b in xs with acc = 100 do acc + b\n\
fn t_early_fold() -> i64 =\n\
  let xs: array<i64, 3> = [1, 2, 3] in\n\
  fold b in xs with acc = 0 do (if b == 2 then return 99 else acc + b)\n\
fn t_static() -> i64 = to_i64(b\"hello\"[1]) + len(b\"hello\")\n\
fn t_fold_slice() -> i64 =\n\
  let s: slice<'static, u8> = b\"abc\" in\n\
  fold b in s with acc = 0 do acc + to_i64(b)\n\
fn flip(p: P) -> P = { x: p.x + 1, y: p.y }\n\
fn t_agg_call() -> i64 =\n\
  let p: P = flip(flip({ x: 5, y: 2 })) in\n\
  p.x\n";

#[test]
fn aggregate_programs_match_the_rust_evaluator() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let exe = evaluator();

    // The Stage-3 acceptance examples: the tokenizer (arrays + early exit
    // through recursion) and a sha256 digest word (records, loops with
    // record accumulators, array_set stores, the aggregate call ABI).
    let tok = temp.path().join("tok.sqlite");
    run(&["init", path(&tok)]);
    run(&["import", path(&tok), "examples/v3/tokenizer.cdb"]);
    for entry in ["tokenize_ok", "tokenize_bad", "tokenize_empty"] {
        assert_three_way(temp.path(), exe, &tok, entry);
    }
    let sha = temp.path().join("sha.sqlite");
    run(&["init", path(&sha)]);
    run(&["import", path(&sha), "examples/v3/sha256.cdb"]);
    assert_three_way(temp.path(), exe, &sha, "digest_0");

    // Per-feature aggregate coverage: nested records, enum payloads through
    // case (with a default arm), arrays with runtime indices, fill +
    // array_set, fold (including an early return from its body), static
    // data + slices, and aggregate params/returns (hidden return slot +
    // indirect param copies).
    let db = import_fixture(temp.path(), "aggregate", AGGREGATE_FIXTURE);
    for entry in [
        "t_record",
        "t_enum",
        "t_array",
        "t_fill_set",
        "t_fold",
        "t_early_fold",
        "t_static",
        "t_fold_slice",
        "t_agg_call",
    ] {
        assert_three_way(temp.path(), exe, &db, entry);
    }

    // An entry with params stays outside the execution protocol.
    let scan_cir = emit_cir(temp.path(), &tok, "scan");
    let (code, stdout) = run_evaluator(exe, &scan_cir);
    assert_eq!(
        (code, stdout.trim()),
        (101, "trap:entry_params"),
        "an entry with params is not executable"
    );
}

#[test]
fn stage4_frontier_is_pinned_fail_closed() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let exe = evaluator();

    // The heap ops await Stage 4; the expectation FLIPS when it lands.
    let db = import_fixture(
        temp.path(),
        "boxed",
        "record Node {\n  v: i64\n}\n\
         fn t_box() -> i64 effects[alloc] =\n\
           let b: box<Node> = box_new({ v: 41 }) in\n\
           let n: Node = unbox(b) in\n\
           n.v + 1\n",
    );
    let cir = emit_cir(temp.path(), &db, "t_box");
    let (code, stdout) = run_evaluator(exe, &cir);
    assert_eq!(
        (code, stdout.trim()),
        (101, "trap:unsupported_op"),
        "box programs await Stage 4 heap ops"
    );
}

#[test]
fn non_cir_input_fails_closed() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let exe = evaluator();

    let empty = temp.path().join("empty.bin");
    std::fs::write(&empty, b"").unwrap();
    let (code, stdout) = run_evaluator(exe, &empty);
    assert_eq!((code, stdout.as_str()), (65, ""), "empty input");

    let garbage = temp.path().join("garbage.bin");
    std::fs::write(&garbage, b"this is definitely not a CIR artifact").unwrap();
    let (code, stdout) = run_evaluator(exe, &garbage);
    assert_eq!((code, stdout.as_str()), (65, ""), "non-CIR input");
}
