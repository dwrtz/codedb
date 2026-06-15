// Phase 15 (ladder rung A): the self-hosted front-end (docs/PLAN_V3.md Phase 15a).
//
// Each .cdb stage object is imported, verified, built to a NATIVE binary, fed
// bytes on stdin, and gated against a Rust determinism-oracle reference:
//
//   compiler/front/lex.cdb     tokenizes like src/expr.rs::lex and prints the
//                              token-stream probe `tokens <count> fnv32 <digest>`,
//                              == codedb::token_probe on the full committed corpus
//                              (incl. string/byte-string literals).
//   compiler/front/sha256.cdb  general multi-block SHA-256 of stdin -> hex digest,
//                              == codedb::sha256_hex; its obj_hash entry frames the
//                              object preimage and reproduces hash_object_canonical
//                              (gated against emit-objects dumps).
//   compiler/front/import.cdb  parses `fn main() -> i64 = <expr>` (15a.2: integer
//                              arithmetic with `+ - * /`, precedence + associativity),
//                              builds the typed Expression tree + the canonical
//                              objects bottom-up, and emits a ProgramRoot hash == the
//                              Rust importer's root for the same source.
//
// The 15a.0 substrate (`emit-objects`, the importer's object/root oracle) is also
// pinned here for determinism across an independent rebuild.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::OnceLock;

use assert_cmd::Command;
use tempfile::{TempDir, tempdir};

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

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

/// Import std/fmt.cdb + the lexer, verify, and build the native lexer binary —
/// once per test process (the tests share the artifact).
fn lexer() -> &'static Path {
    static LEXER: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    LEXER
        .get_or_init(|| {
            let temp = tempdir().unwrap();
            let db = temp.path().join("selfhost-frontend.sqlite");
            run(&["init", path(&db)]);
            run(&["import", path(&db), "std/fmt.cdb"]);
            run(&["import", path(&db), "compiler/front/lex.cdb"]);
            run(&["verify", path(&db)]);
            let exe = temp.path().join("lex-bin");
            run(&["build", path(&db), "main", "--out", path(&exe)]);
            (temp, exe)
        })
        .1
        .as_path()
}

/// Run the native lexer with `source` on stdin; return its trimmed stdout.
fn run_lexer(exe: &Path, source: &str) -> String {
    let mut child = StdCommand::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn lexer");
    child
        .stdin
        .take()
        .expect("lexer stdin")
        .write_all(source.as_bytes())
        .expect("write lexer stdin");
    let output = child.wait_with_output().expect("wait lexer");
    assert!(output.status.success(), "lexer exited non-zero: {output:?}");
    String::from_utf8(output.stdout)
        .expect("utf8 lexer stdout")
        .trim()
        .to_string()
}

/// Assert the .cdb lexer's probe equals the Rust `token_probe` for `source`.
fn assert_probe(exe: &Path, source: &str) {
    let got = run_lexer(exe, source);
    let want = codedb::token_probe(source)
        .expect("token_probe")
        .trim()
        .to_string();
    assert_eq!(got, want, "lexer probe mismatch for source: {source:?}");
}

/// Import + verify + build the native SHA-256 hasher — once per test process.
fn hasher() -> &'static Path {
    static HASHER: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    HASHER
        .get_or_init(|| {
            let temp = tempdir().unwrap();
            let db = temp.path().join("selfhost-sha256.sqlite");
            run(&["init", path(&db)]);
            run(&["import", path(&db), "compiler/front/lib.cdb"]);
            run(&["import", path(&db), "compiler/front/sha256.cdb"]);
            run(&["verify", path(&db)]);
            let exe = temp.path().join("sha-bin");
            run(&["build", path(&db), "main", "--out", path(&exe)]);
            (temp, exe)
        })
        .1
        .as_path()
}

/// Import + verify + build the `obj_hash` entry of the hasher (the object-hash
/// wrapper, hash_object_canonical) — once per test process.
fn obj_hasher() -> &'static Path {
    static OBJ_HASHER: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    OBJ_HASHER
        .get_or_init(|| {
            let temp = tempdir().unwrap();
            let db = temp.path().join("selfhost-objhash.sqlite");
            run(&["init", path(&db)]);
            run(&["import", path(&db), "compiler/front/lib.cdb"]);
            run(&["import", path(&db), "compiler/front/sha256.cdb"]);
            run(&["verify", path(&db)]);
            let exe = temp.path().join("obj-bin");
            run(&["build", path(&db), "obj_hash", "--out", path(&exe)]);
            (temp, exe)
        })
        .1
        .as_path()
}

/// Run the native hasher with `bytes` on stdin; return its trimmed hex digest.
fn run_hasher(exe: &Path, bytes: &[u8]) -> String {
    let mut child = StdCommand::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hasher");
    child
        .stdin
        .take()
        .expect("hasher stdin")
        .write_all(bytes)
        .expect("write hasher stdin");
    let output = child.wait_with_output().expect("wait hasher");
    assert!(output.status.success(), "hasher exited non-zero: {output:?}");
    String::from_utf8(output.stdout)
        .expect("utf8 hasher stdout")
        .trim()
        .to_string()
}

/// Assert the .cdb hasher's digest equals the Rust `sha256_hex` reference.
fn assert_sha(exe: &Path, bytes: &[u8]) {
    let got = run_hasher(exe, bytes);
    let want = codedb::sha256_hex(bytes);
    assert_eq!(got, want, "sha256 mismatch for a {}-byte input", bytes.len());
}

#[test]
fn lexer_probe_matches_rust_on_minimal_corpus() {
    if !can_build_default_native_target() {
        return;
    }
    let exe = lexer();
    // The empty stream (only Eof), each single-token class, and a two-char symbol.
    assert_probe(exe, "");
    assert_probe(exe, "a");
    assert_probe(exe, "1");
    assert_probe(exe, "+");
    assert_probe(exe, "ab");
    assert_probe(exe, "->");
    // Whitespace handling, identifiers with digits/underscores, and a comment.
    assert_probe(exe, "   \t \n  ");
    assert_probe(exe, "a_b1 c2_D // trailing comment to end of line");
    // Decimal and 0x-hex numbers adjacent to operators.
    assert_probe(exe, "0xFF + 0x1a - 42");
    // Every two-char symbol the lexer recognizes, back to back.
    assert_probe(exe, "-> == != <= >= && || :: => ..");
    // A realistic single-line and multi-line program (recursion, bools, calls).
    assert_probe(exe, "fn main() -> i64 = 1 + 2");
    assert_probe(exe, "let x: i64 = foo(1, 2) in x * 3 - 4");
    assert_probe(
        exe,
        "fn f(n: i64) -> i64 = if n <= 1 then 1 else n * f(n - 1)\n\
         fn g(n: i64) -> bool = true && false || n != 0\n",
    );
    // String literals, folded over their DECODED bytes (escapes resolved).
    assert_probe(exe, r#""hello world""#);
    assert_probe(exe, r#""escapes: \n \t \" \\ done""#);
    assert_probe(exe, r#"fn greet() -> string = "hi, there""#);
    // Byte-string literals, incl. the \0 and \xHH byte escapes.
    assert_probe(exe, r#"b"0""#);
    assert_probe(exe, r#"b"bytes \x1f \x00 \0 \n \t end""#);
    // A string adjacent to other tokens and a comment containing quotes/backslash.
    assert_probe(exe, r#"let s: string = "a" in print(s) // a "quoted" \ note"#);
}

#[test]
fn lexer_probe_matches_rust_on_the_committed_corpus() {
    if !can_build_default_native_target() {
        return;
    }
    let exe = lexer();
    // The self-hosted lexer tokenizes every committed .cdb source identically to
    // the Rust lexer — including string/byte-string literals, the full std and
    // examples corpus, the 1700-line evaluator, and the lexer itself (dogfood).
    for file in [
        "std/core.cdb",
        "std/mem.cdb",
        "std/result.cdb",
        "std/alloc.cdb",
        "std/string.cdb",
        "std/fmt.cdb",
        "std/io.cdb",
        "examples/v3/tokenizer.cdb",
        "examples/v3/sha256.cdb",
        "compiler/eval/eval.cdb",
        "compiler/front/lex.cdb",
    ] {
        let source = std::fs::read_to_string(file).unwrap_or_else(|_| panic!("read {file}"));
        assert_probe(exe, &source);
    }
}

#[test]
fn sha256_matches_reference_across_lengths_and_blocks() {
    // The content-addressing keystone (SPEC_V3 §5): the self-hosted hasher must
    // compute SHA-256 of arbitrary bytes byte-for-byte like the reference, or the
    // importer can never reproduce object/root hashes. Covers empty input, the
    // padding edges (55 bytes fits one block with its length; 56 forces a second),
    // multi-block messages, and all 256 byte values.
    if !can_build_default_native_target() {
        return;
    }
    let exe = hasher();
    assert_sha(exe, b"");
    assert_sha(exe, b"abc");
    assert_sha(exe, b"The quick brown fox jumps over the lazy dog");
    for len in [1usize, 54, 55, 56, 57, 63, 64, 65, 119, 127, 128, 129, 200, 1000] {
        assert_sha(exe, &vec![b'a'; len]);
    }
    // Every byte value, incl. 0x00 and 0xff, spanning blocks — canonical object
    // payloads are arbitrary bytes, so the hasher must be byte-exact.
    let all_bytes: Vec<u8> = (0..=255u8).collect();
    assert_sha(exe, &all_bytes);
    // A canonical-JSON-payload-shaped input (the shape object hashing will feed it).
    assert_sha(
        exe,
        br#"{"expr_kind":"binary","left":"sha256:a","op":"+","right":"sha256:b","type":"sha256:i64"}"#,
    );
}

#[test]
fn obj_hash_reproduces_hash_object_canonical_for_real_objects() {
    // The content-addressing core, end to end: the .cdb obj_hash wrapper frames
    // OBJECT_DOMAIN || kind || \0 || schema || \0 || payload and SHA-256s it, so it
    // must reproduce src/store.rs::hash_object_canonical. Every line of an
    // `emit-objects` dump is a real (kind, schema, canonical payload -> hash) case,
    // so this proves the .cdb computes the SAME object hashes CodeDB does.
    if !can_build_default_native_target() {
        return;
    }
    let exe = obj_hasher();
    // A program with varied object kinds (a record, an enum, several functions)
    // so the dump spans short and long canonical payloads.
    let temp = tempdir().unwrap();
    let db = temp.path().join("prog.sqlite");
    let src = temp.path().join("prog.cdb");
    std::fs::write(
        &src,
        "record Point { x: i64  y: i64 }\n\
         enum Opt { none: unit  some: i64 }\n\
         fn add(a: i64, b: i64) -> i64 = a + b\n\
         fn main() -> i64 = add(40, 2)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let dump_path = temp.path().join("dump.txt");
    run(&["emit-objects", path(&db), "--out", path(&dump_path)]);
    let dump = std::fs::read_to_string(&dump_path).unwrap();

    let mut checked = 0usize;
    for line in dump.lines() {
        // `<hash>\t<kind>\t<schema>\t<payload>`; the trailing `root <hash>` line
        // has no tabs and is skipped.
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() != 4 {
            continue;
        }
        let (hash, kind, schema, payload) = (cols[0], cols[1], cols[2], cols[3]);
        let input = format!("{kind}\n{schema}\n{payload}");
        let got = run_hasher(exe, input.as_bytes());
        assert_eq!(
            got, hash,
            "obj_hash mismatch for a {kind} object (schema {schema})"
        );
        checked += 1;
    }
    assert!(
        checked >= 10,
        "the dump should carry several objects; only checked {checked}"
    );
}

/// Import + verify + build the self-hosted importer (the minimal-grammar
/// source -> root-hash program) — once per test process.
fn importer() -> &'static Path {
    static IMPORTER: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    IMPORTER
        .get_or_init(|| {
            let temp = tempdir().unwrap();
            let db = temp.path().join("selfhost-import.sqlite");
            run(&["init", path(&db)]);
            run(&["import", path(&db), "compiler/front/lib.cdb"]);
            run(&["import", path(&db), "compiler/front/import.cdb"]);
            run(&["verify", path(&db)]);
            let exe = temp.path().join("import-bin");
            run(&["build", path(&db), "main", "--out", path(&exe)]);
            (temp, exe)
        })
        .1
        .as_path()
}

#[test]
fn importer_reproduces_the_root_hash_for_the_minimal_grammar() {
    // The rung-A importer milestone (15a.3) on its smallest input: the .cdb
    // importer reads `fn main() -> i64 = <int>`, builds the six content-addressed
    // objects with their exact canonical payloads, chains hash_object over them,
    // and emits a ProgramRoot hash that EQUALS the Rust importer's root for the
    // same source — proving the .cdb computes the same program identity CodeDB does.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    for value in ["0", "1", "2", "42", "1000000", "9223372036854775807"] {
        let source = format!("fn main() -> i64 = {value}\n");
        // The Rust importer's root for this source.
        let db = temp.path().join(format!("ref-{value}.sqlite"));
        let src = temp.path().join(format!("ref-{value}.cdb"));
        std::fs::write(&src, &source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        // The self-hosted importer's root from the same source on stdin.
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_integer_expressions() {
    // 15a.2 (axis 1): the real expression parser. The .cdb importer scans and parses
    // `fn main() -> i64 = <expr>` where <expr> is an integer expression over the full
    // i64 operator set — prefix unary `- ~`; `* / %`; `+ -`; shifts `<< >>`; bitwise
    // `& ^ |` — with the Rust precedence and left-associativity, building the typed
    // Expression tree bottom-up (each parse function returns the content hash of the
    // object it built). Its ProgramRoot hash must equal the Rust importer's for the
    // same source — an exact gate, since any precedence, associativity, operator
    // spelling, or canonical-payload error changes a subtree's object hash and so the
    // root.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let exprs = [
        // arithmetic: precedence + associativity
        "1 + 2",
        "1+2",
        "1 + 2 * 3",
        "2 * 3 + 4",
        "10 - 2 - 3",
        "7 + 6 * 5 - 4 / 2",
        "1*2+3*4+5*6",
        "100 - 10 * 9 + 1",
        // modulo
        "10 % 3",
        "17 % 5 % 2",
        "10 - 2 * 3 % 4",
        // prefix unary (single, double, mixed with binary)
        "-5",
        "~5",
        "1 + -2",
        "1 - - 3",
        "~-5",
        "-~-1",
        "-2 * 3 + 1",
        "~0 & 255",
        // shifts (and their precedence vs +)
        "1 << 4",
        "256 >> 2",
        "1 << 2 + 3",
        "1 + 2 << 3",
        "100 >> 1 >> 1",
        // bitwise and the &-^-| precedence chain
        "12 & 10",
        "12 | 3",
        "5 ^ 3",
        "1 | 2 & 3",
        "1 ^ 2 & 3",
        "6 & 3 ^ 1",
        "255 & 15 ^ 1",
        "1 << 4 | 2",
        "2 + 3 << 1",
        "1 | 2 | 4 | 8",
        // hex literals (0x…): the canonical `value` is the raw source slice, so a
        // wrong scan (digits, case, or the 0x prefix) changes the literal's hash.
        "0xff",
        "0x0",
        "0xFF",
        "0xdeadbeef",
        "0xff + 1",
        "0x10 * 0x10",
        "0xff & 0x0f | 0x80",
        "1 + 0xa * 2",
    ];
    for (i, expr) in exprs.iter().enumerate() {
        let source = format!("fn main() -> i64 = {expr}\n");
        // The Rust importer's root for this source.
        let db = temp.path().join(format!("ref-int-{i}.sqlite"));
        let src = temp.path().join(format!("ref-int-{i}.cdb"));
        std::fs::write(&src, &source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        // The self-hosted importer's root from the same source on stdin.
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer arithmetic root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_bool_expressions() {
    // 15a.2 (axis 1, type inference): the parser infers each expression's type
    // (i64 vs bool) and threads it into every node's `type` field, and reads the
    // declared return type from the header. Comparisons (`< > <= >= == !=`) take i64
    // operands and yield bool; bool literals `true`/`false`, logical `&& || !`, and
    // the bool-returning signature round out the bool surface. Each source is the
    // COMPLETE program (with its return type) — only type-valid programs are gated
    // (the .cdb importer does not type check, so the Rust importer is the authority
    // on what is well-typed). Root-hash equality is exact: a wrong inferred type
    // changes a node's `type` hash and the root.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // i64-returning regressions (the previous increments still hold)
        "fn main() -> i64 = 42",
        "fn main() -> i64 = 1 + 2 * 3",
        "fn main() -> i64 = 1 << 4 | 2",
        "fn main() -> i64 = ~0 & 255",
        // each comparison operator, bool-returning
        "fn main() -> bool = 1 < 2",
        "fn main() -> bool = 5 > 2",
        "fn main() -> bool = 1 <= 1",
        "fn main() -> bool = 2 >= 3",
        "fn main() -> bool = 3 == 3",
        "fn main() -> bool = 3 != 4",
        // arithmetic/bitwise/shift operands feeding a comparison (i64 -> bool)
        "fn main() -> bool = 1 + 1 == 2",
        "fn main() -> bool = 2 * 3 > 5",
        "fn main() -> bool = 10 % 3 == 1",
        "fn main() -> bool = 1 << 1 == 2",
        "fn main() -> bool = 1 + 2 == 3 - 1",
        "fn main() -> bool = 100 >> 2 < 30",
        "fn main() -> bool = -3 < 0",
        // bool literals and logical `! && ||`, with their precedence
        "fn main() -> bool = true",
        "fn main() -> bool = false",
        "fn main() -> bool = !true",
        "fn main() -> bool = true && false",
        "fn main() -> bool = true || false",
        "fn main() -> bool = true && false || true",
        "fn main() -> bool = true || false && false",
        "fn main() -> bool = 1 < 2 && 3 < 4",
        "fn main() -> bool = 1 == 1 && 2 == 2",
        "fn main() -> bool = true && 1 == 1",
        "fn main() -> bool = !true || false",
        "fn main() -> bool = !true && !false",
        "fn main() -> bool = 1 + 1 == 2 && 3 * 2 > 5",
    ];
    for (i, source_expr) in sources.iter().enumerate() {
        let source = format!("{source_expr}\n");
        // The Rust importer's root for this source.
        let db = temp.path().join(format!("ref-bool-{i}.sqlite"));
        let src = temp.path().join(format!("ref-bool-{i}.cdb"));
        std::fs::write(&src, &source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        // The self-hosted importer's root from the same source on stdin.
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer comparison/bool root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_if_expressions() {
    // 15a.2 (axis 1): `if cond then a else b`. `if` is an expression whose cond/then/
    // else are full expressions, so the parser becomes mutually recursive (atom ->
    // if -> the binary ladder -> atom) and the .cdb compiles as one recursion group.
    // The if node's type is the then/else branch type, which threads up like any
    // other. Covers bool and i64 results, complex (logical/comparison) conditions,
    // nested `if` in the then-branch, and an else-if chain. Root-hash equality is
    // exact. (`if` in operand position, e.g. `1 + if ...`, is a Rust parse error and
    // out of scope — the parser is intentionally more permissive there for now.)
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        "fn main() -> i64 = if 1 < 2 then 10 else 20",
        "fn main() -> i64 = if true then 1 else 2",
        "fn main() -> i64 = if 1 == 1 then 100 else 0",
        "fn main() -> bool = if 1 < 2 then true else false",
        "fn main() -> bool = if true then false else true",
        "fn main() -> i64 = if 1 + 1 == 2 && 3 > 0 then 9 else 8",
        "fn main() -> i64 = if 1 < 2 then if 3 < 4 then 5 else 6 else 7",
        "fn main() -> i64 = if 1 < 2 then 10 else if 3 < 4 then 20 else 30",
        "fn main() -> i64 = if 5 % 2 == 1 then 1 << 3 else 0",
    ];
    for (i, source_expr) in sources.iter().enumerate() {
        let source = format!("{source_expr}\n");
        let db = temp.path().join(format!("ref-if-{i}.sqlite"));
        let src = temp.path().join(format!("ref-if-{i}.cdb"));
        std::fs::write(&src, &source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer if-expression root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_let_expressions() {
    // 15a.2 (axis 1): `let IDENT: TYPE = value in body` plus identifier (local
    // variable) references. Identifiers resolve to a `local_ref` by de-Bruijn depth
    // (innermost binding = 0), tracked through a lexical scope threaded down the
    // parser and extended only into a let body; a shadowing binding resolves to the
    // nearest one, and the ref is typed by the binding it names. The let's own type is
    // its body's type, the value is parsed in the outer scope and the body in the
    // extended scope. Root-hash equality is exact: a wrong depth, a wrong binding/ref
    // type, a misplaced scope boundary, or a non-canonical payload changes a subtree's
    // object hash and so the root. Covers single/nested/three-level bindings, shadowing,
    // a value that references an outer binding, bool bindings, lets nested with `if`,
    // multi-character identifiers, and a deep nest whose depths span two decimal digits.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let mut sources: Vec<String> = vec![
        // single binding, body is the variable (depth 0)
        "fn main() -> i64 = let x: i64 = 5 in x".to_string(),
        // binding used inside an operator expression
        "fn main() -> i64 = let x: i64 = 10 in x * x + 1".to_string(),
        // no spaces around the `:` / `=`
        "fn main() -> i64 = let x:i64=3 in x".to_string(),
        // two-level nest: a = depth 1, b = depth 0
        "fn main() -> i64 = let a: i64 = 1 in let b: i64 = 2 in a + b".to_string(),
        // three-level nest: a = 2, b = 1, c = 0
        "fn main() -> i64 = let a: i64 = 1 in let b: i64 = 2 in let c: i64 = 3 in a + b + c"
            .to_string(),
        // shadowing: the body's `x` resolves to the inner binding (depth 0)
        "fn main() -> i64 = let x: i64 = 1 in let x: i64 = 2 in x".to_string(),
        // a binding's value references an outer binding
        "fn main() -> i64 = let a: i64 = 5 in let b: i64 = a + 1 in b".to_string(),
        // a chain of single-variable bindings
        "fn main() -> i64 = let a: i64 = 1 in let b: i64 = a in let c: i64 = b in c".to_string(),
        // bool binding, bool body
        "fn main() -> bool = let p: bool = true in p".to_string(),
        // bool binding whose value is a comparison
        "fn main() -> bool = let p: bool = 1 < 2 in p".to_string(),
        // i64 bindings feeding a bool body
        "fn main() -> bool = let a: i64 = 1 in let b: i64 = 2 in a < b".to_string(),
        "fn main() -> bool = let x: i64 = 5 in let y: i64 = 5 in x == y && x > 0".to_string(),
        // let interacting with if (binding used in the condition / branches)
        "fn main() -> i64 = let cond: bool = 1 < 2 in if cond then 100 else 200".to_string(),
        "fn main() -> i64 = let n: i64 = 7 in if n > 5 then n - 5 else n".to_string(),
        // multi-character identifiers, one referencing the other
        "fn main() -> i64 = let outer: i64 = 100 in let inner: i64 = outer * 2 in outer + inner"
            .to_string(),
    ];
    // A deep nest (let v0=0 in let v1=1 in ... in v0): the body references the
    // outermost binding, so its depth is N-1 — past 9 it spans two decimal digits,
    // exercising the bare-integer `depth` renderer and the scope past ten entries.
    // (Capped at the scope's combined param+binding capacity.)
    for n in [11usize, 12] {
        let mut s = String::from("fn main() -> i64 = ");
        for i in 0..n {
            s.push_str(&format!("let v{i}: i64 = {i} in "));
        }
        s.push_str("v0");
        sources.push(s);
    }
    for (i, source_expr) in sources.iter().enumerate() {
        let source = format!("{source_expr}\n");
        let db = temp.path().join(format!("ref-let-{i}.sqlite"));
        let src = temp.path().join(format!("ref-let-{i}.cdb"));
        std::fs::write(&src, &source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer let-expression root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_parameters() {
    // 15a.2 (axis 1): function parameters and `param_ref`. The header now parses a
    // parameter list `(a: i64, b: bool, ...)`, which feeds the FunctionSignature's
    // `params` (a list of Type hashes) and the ProgramRoot's `param_names`; an in-body
    // reference to a parameter becomes a `param_ref` by its positional index, typed by
    // that parameter. Parameters and `let` bindings are SEPARATE namespaces resolved
    // from one combined scope (params at [0, np), lets pushed on top): a let is checked
    // first (so it shadows a same-named parameter) and its de-Bruijn depth does not count
    // parameters. Root-hash equality is exact, so a wrong param index/type, a wrong
    // signature param list or param_names, or a let/param resolution mix-up changes the
    // root. Covers single/multiple/out-of-order params, mixed i64/bool param and return
    // types, params mixed with lets (shadowing, a value using params, a param referenced
    // from inside nested lets), and a parameter index spanning two decimal digits.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let mut sources: Vec<String> = vec![
        // single parameter, body is the parameter (index 0)
        "fn main(a: i64) -> i64 = a".to_string(),
        "fn main(x: i64) -> i64 = x * x + 2 * x + 1".to_string(),
        // multiple parameters, in and out of declaration order
        "fn main(a: i64, b: i64) -> i64 = a + b".to_string(),
        "fn main(a: i64, b: i64) -> i64 = b - a".to_string(),
        "fn main(a: i64, b: i64, c: i64) -> i64 = a * b + c".to_string(),
        // mixed parameter / return types; a bool parameter feeding `if`
        "fn main(a: i64, b: bool) -> i64 = if b then a else 0".to_string(),
        "fn main(n: i64) -> bool = if n > 0 then true else false".to_string(),
        "fn main(p: bool, q: bool) -> bool = p && q || !p".to_string(),
        // parameters mixed with let bindings
        "fn main(a: i64) -> i64 = let x: i64 = 5 in a + x".to_string(),
        // a let shadows a same-named parameter (body resolves to the let, depth 0)
        "fn main(a: i64) -> i64 = let a: i64 = 5 in a".to_string(),
        // a binding's value references parameters
        "fn main(a: i64, b: i64) -> i64 = let c: i64 = a + b in c".to_string(),
        // a parameter referenced from inside nested lets (still a param_ref, not local)
        "fn main(a: i64) -> i64 = let x: i64 = 1 in let y: i64 = 2 in a + x + y".to_string(),
        "fn main(base: i64) -> i64 = let sq: i64 = base * base in let cube: i64 = sq * base in sq + cube"
            .to_string(),
        "fn main(a: i64, b: i64) -> bool = let s: i64 = a + b in s > 10 && s < 100".to_string(),
        // a parameter used inside a let nested in an if branch
        "fn main(n: i64) -> i64 = if n > 0 then let d: i64 = n - 1 in d else 0".to_string(),
    ];
    // Eleven parameters a..k: a reference to the last is `param_ref` index 10 — past 9
    // it spans two decimal digits, exercising the bare-integer `index` renderer.
    let params11 = "a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64, i: i64, j: i64, k: i64";
    sources.push(format!("fn main({params11}) -> i64 = k"));
    sources.push(format!("fn main({params11}) -> i64 = a + k"));
    for (i, source_expr) in sources.iter().enumerate() {
        let source = format!("{source_expr}\n");
        let db = temp.path().join(format!("ref-param-{i}.sqlite"));
        let src = temp.path().join(format!("ref-param-{i}.cdb"));
        std::fs::write(&src, &source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer parameter root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_sized_integers() {
    // 15a.2 (axis 1): sized integer types (u8/u16/u32/u64/i8/i16/i32) and the
    // EXPECTATION PROPAGATION they force. The importer now threads an expected type
    // down the parser: an integer literal takes the expected type if one is set (else
    // i64); a binding/return/param annotation supplies it; arithmetic propagates it to
    // both operands; a comparison's operands unify (left informs right). So
    // `fn main() -> u8 = 200` types `200` as u8, `let x: u32 = 1 in x + 2` types the `2`
    // as u32, and `fn main(a: u32, b: u32) -> u32 = a + b` is all u32. Root-hash
    // equality is exact, so a wrong inferred width anywhere changes a Type hash and the
    // root. Only programs whose literal types are fixed top-down or by a concrete
    // sibling are in scope (a bare literal LEFT of a concretely-typed operand, e.g.
    // `1 < a`, is not yet unified — out of scope, as are cast builtins).
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // a literal at each width, driven by the return type
        "fn main() -> u8 = 200",
        "fn main() -> u16 = 5",
        "fn main() -> u32 = 5",
        "fn main() -> u64 = 5",
        "fn main() -> i8 = 5",
        "fn main() -> i16 = 5",
        "fn main() -> i32 = 5",
        // hex literal in a sized context
        "fn main() -> u32 = 0xff",
        // propagation through arithmetic / bitwise / shift
        "fn main() -> u32 = 1 + 2 * 3",
        "fn main() -> u8 = 255 & 0x0f",
        "fn main() -> u32 = 1 << 4 | 2",
        // sized parameters feeding arithmetic / unary
        "fn main(a: u32, b: u32) -> u32 = a + b",
        "fn main(a: u32) -> u32 = a * a + 1",
        "fn main(a: u8, b: u8) -> u8 = a + b * 2",
        "fn main(a: u32) -> u32 = ~a",
        "fn main(a: i32) -> i32 = -a",
        "fn main() -> u32 = ~0 & 255",
        "fn main(a: i16, b: i16) -> i16 = a + b",
        "fn main(a: u64) -> u64 = a << 8 | 0xff",
        // sized let bindings, propagating into the body
        "fn main() -> u32 = let x: u32 = 5 in x",
        "fn main() -> u8 = let x: u8 = 1 in x + 2",
        "fn main(a: u32) -> u32 = let b: u32 = a + 1 in b * 2",
        "fn main() -> u64 = let lo: u64 = 0xff in let hi: u64 = 0xff00 in lo + hi",
        // sized comparisons (uniform or concrete-left) yielding bool
        "fn main(a: u32, b: u32) -> bool = a < b",
        "fn main(a: u32) -> bool = a < 10",
        "fn main(a: u32) -> bool = a == 5 && a > 0",
        "fn main(a: u8, b: u8) -> bool = a + b == 255",
    ];
    for (i, source_expr) in sources.iter().enumerate() {
        let source = format!("{source_expr}\n");
        let db = temp.path().join(format!("ref-sized-{i}.sqlite"));
        let src = temp.path().join(format!("ref-sized-{i}.cdb"));
        std::fs::write(&src, &source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer sized-integer root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_int_casts() {
    // 15a.2 (axis 1): the integer cast builtins `to_u8`/`to_u16`/…/`to_i64`. A
    // `to_<type>(expr)` builds an `int_cast` Expression whose `source_type` is the
    // argument's own inferred type, `type` is the target named by the cast (regardless
    // of the surrounding expectation), and `value` is the argument. The argument is a
    // full expression parsed with NO expectation (it keeps its own type, so a bare
    // literal there is i64). Root-hash equality is exact, so a wrong source/target type
    // or a misparsed argument changes the cast's hash. Covers every target width, a hex
    // and an arithmetic argument, a parameter argument, nested casts, and casts inside
    // expressions and `let` bindings.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        "fn main() -> u8 = to_u8(255)",
        "fn main() -> u16 = to_u16(1000)",
        "fn main() -> u32 = to_u32(5)",
        "fn main() -> u64 = to_u64(5)",
        "fn main() -> i8 = to_i8(5)",
        "fn main() -> i32 = to_i32(5)",
        "fn main() -> i64 = to_i64(5)",
        // a hex and an arithmetic argument (the argument keeps its own type, i64 here)
        "fn main() -> u8 = to_u8(0xff)",
        "fn main() -> u8 = to_u8(1 + 2)",
        "fn main() -> u32 = to_u32(10 * 10)",
        // casts as operands inside an expression
        "fn main() -> u32 = to_u32(5) + 1",
        "fn main() -> u8 = to_u8(255) & 0x0f",
        // a parameter argument (source_type is the parameter's type)
        "fn main(a: i64) -> u8 = to_u8(a)",
        "fn main(a: u32) -> i64 = to_i64(a)",
        // nested casts
        "fn main() -> i64 = to_i64(to_u32(5))",
        "fn main() -> u8 = to_u8(to_i64(255))",
        // casts in let bindings
        "fn main() -> u8 = let x: u8 = to_u8(200) in x",
        "fn main(a: u32) -> u64 = let b: u64 = to_u64(a) in b + 1",
    ];
    for (i, source_expr) in sources.iter().enumerate() {
        let source = format!("{source_expr}\n");
        let db = temp.path().join(format!("ref-cast-{i}.sqlite"));
        let src = temp.path().join(format!("ref-cast-{i}.cdb"));
        std::fs::write(&src, &source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer int-cast root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_two_functions() {
    // 15a.4 (axis 2): a MULTI-FUNCTION program. For 2+ functions the ProgramRoot is no
    // longer one symbol over genesis — each non-first symbol's SymbolBirth carries the
    // running history hash at its creation, so reproducing the root means reproducing the
    // migration/history chain: order the functions canonically (alphabetical by name),
    // build the first's single-symbol ProgramRoot, fold it through the first migration
    // (a create_function operation with a RAW-AST body plus root_is_current/
    // name_is_available preconditions and root_exists/function_source_matches
    // postconditions, hashed under MIGRATION_DOMAIN then HISTORY_DOMAIN) into the running
    // history, then build the final two-symbol ProgramRoot with the second symbol born at
    // that history. Root-hash equality is exact, so a wrong canonical order, ordinal,
    // migration/history preimage, or birth-history chaining changes the root. Keystone
    // scope: two no-param i64-returning functions with literal bodies. Covers source order
    // both == and != the canonical (alphabetical) order, both-non-main names, and assorted
    // literal values.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // source order already canonical (helper < main)
        "fn helper() -> i64 = 1\nfn main() -> i64 = 2\n",
        // source order != canonical: the importer must sort to aaa(0), zzz(1)
        "fn zzz() -> i64 = 1\nfn aaa() -> i64 = 2\n",
        // `main` appears first in source but sorts after `helper`
        "fn main() -> i64 = 9\nfn helper() -> i64 = 7\n",
        // both names non-main, already alphabetical
        "fn aaa() -> i64 = 3\nfn bbb() -> i64 = 4\n",
        // larger / repeated literal values (value threads through both objects + the chain)
        "fn helper() -> i64 = 123456\nfn main() -> i64 = 789\n",
        "fn helper() -> i64 = 5\nfn main() -> i64 = 5\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-two-{i}.sqlite"));
        let src = temp.path().join(format!("ref-two-{i}.cdb"));
        std::fs::write(&src, source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer two-function root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_two_function_rich_bodies() {
    // 15a.4 (axis 2): the RAW-AST serializer. The two-function migration body is the dual
    // (raw) serialization of the canonical-FIRST function's body, distinct from the typed
    // Expression objects — so a multi-function program whose first body is ANY expression
    // (not just a literal) only reproduces the root if that raw AST is byte-exact. A second,
    // type-free recursive-descent parser produces it (mirroring the typed precedence ladder:
    // operators, unary, bool, `if`, `let` + by-name refs, casts), and the chain builders
    // embed it in the operation AND the function_source_matches postcondition with the
    // function's real return-type name. The forms were spiked against `codedb history --json`
    // first (binary/unary/if key orders, `param_name` refs, `call` casts, bare-bool/raw-text
    // literals). Covers a rich first body across the whole grammar, a rich SECOND body (the
    // typed re-parse path), both rich, and canonical ordering with the rich body sorting
    // second — i64/bool/sized returns throughout. Root-hash equality is exact.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // rich FIRST body (canonical-first => drives the raw serializer), trivial second
        "fn aaa() -> i64 = 1 + 2 * 3 - 4\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = 100 / 5 % 7\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = 6 & 3 ^ 8 | 1\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = 1 << 3 | 4\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> bool = 1 + 1 == 2\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> bool = 1 < 2 && 3 < 4\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> bool = ! true\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = - 5 + 3\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = ~ 7\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> bool = false\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = if true then if false then 1 else 2 else 3\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = let a: i64 = 1 in let b: i64 = a + 2 in a * b\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = let x: i64 = 5 in if x < 3 then x else 9\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> u8 = to_u8(1 + 2)\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> i64 = 0xff & 0x0f\nfn zzz() -> i64 = 0\n",
        "fn aaa() -> u8 = let x: u8 = 5 in x\nfn zzz() -> i64 = 0\n",
        // rich SECOND body (typed-only re-parse path; trivial canonical-first migration)
        "fn aaa() -> i64 = 0\nfn zzz() -> i64 = 1 + 2 * 3\n",
        "fn aaa() -> i64 = 0\nfn zzz() -> i64 = let q: i64 = 1 in q + 2\n",
        // both rich
        "fn aaa() -> i64 = 1 + 2\nfn zzz() -> bool = 3 < 4\n",
        "fn aaa() -> bool = ! false\nfn zzz() -> i64 = to_i64(5)\n",
        // canonical ordering: the rich body is on the function that sorts SECOND
        "fn zzz() -> i64 = 1 + 2 * 3\nfn aaa() -> i64 = 9\n",
        "fn main() -> i64 = 7 + 8\nfn helper() -> i64 = 1 << 2\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-rich-{i}.sqlite"));
        let src = temp.path().join(format!("ref-rich-{i}.cdb"));
        std::fs::write(&src, source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer rich two-function root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_cross_symbol_calls() {
    // 15a.4 (axis 2): a no-argument CROSS-SYMBOL call `<callee>()`. A call introduces a
    // dependency edge, so the canonical order is no longer alphabetical but a TOPOSORT —
    // the callee is created BEFORE its caller (so the caller's typed body can reference the
    // callee's already-determined symbol). The typed `call` node references the callee by
    // content-addressed symbol hash (re-derived: in a two-function DAG the callee is the
    // canonical-first, born at genesis/ordinal 0), typed by the callee's return type (found
    // by re-scanning the source). Reproducing the root exercises the whole chain: detecting
    // the call edge, ordering callee-first, the call expression, AND — the keystone subtlety
    // this first exposed — the ProgramRoot's `names` array being display-name-ordered while
    // param_names/symbols are symbol-hash-ordered (they diverge once toposort != alphabetical).
    // Covers the call when dependency order matches AND contradicts alphabetical, the call in
    // richer expressions (arithmetic/if/let), a bool-returning callee, and a callee with a
    // non-literal body (its migration body exercises the raw serializer). Mutual recursion is a
    // recursion group (create_recursion_group), out of scope here.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // caller `aaa` calls callee `zzz`: dependency order (zzz first) CONTRADICTS alphabetical
        "fn aaa() -> i64 = zzz() + 1\nfn zzz() -> i64 = 5\n",
        "fn aaa() -> i64 = zzz()\nfn zzz() -> i64 = 7\n",
        "fn zzz() -> i64 = 5\nfn aaa() -> i64 = zzz() + 1\n",
        // callee `helper` sorts before caller `main`: dependency order matches alphabetical
        "fn main() -> i64 = helper() * 2\nfn helper() -> i64 = 21\n",
        "fn helper() -> i64 = 21\nfn main() -> i64 = helper() * 2\n",
        // call in richer expressions
        "fn aaa() -> i64 = zzz() + zzz()\nfn zzz() -> i64 = 3\n",
        "fn aaa() -> i64 = if zzz() < 5 then 1 else 2\nfn zzz() -> i64 = 3\n",
        "fn aaa() -> i64 = let x: i64 = zzz() in x + 1\nfn zzz() -> i64 = 9\n",
        // bool-returning callee; callee with a non-literal body (raw serializer in its migration)
        "fn aaa() -> bool = chk() && true\nfn chk() -> bool = true\n",
        "fn aaa() -> i64 = base() + 1\nfn base() -> i64 = 2 * 3 + 4\n",
        "fn aaa() -> i64 = base()\nfn base() -> i64 = let q: i64 = 5 in q * 2\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-call-{i}.sqlite"));
        let src = temp.path().join(format!("ref-call-{i}.cdb"));
        std::fs::write(&src, source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer cross-symbol-call root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_self_recursion() {
    // 15a.4 (axis 2): a SELF-RECURSIVE function. A function whose body calls its own name is a
    // one-member RECURSION GROUP — the importer emits `create_recursion_group` (not
    // create_function): the member's SymbolBirth uses `local_nonce = "recursion_group:0"` (the
    // member ordinal, NOT its name), there is a new `RecursionGroup` object, and the ProgramRoot
    // carries a `recursion_groups` array alongside `symbols`. The self-call resolves to that
    // `recursion_group:0` symbol (the importer marks the body's scope rgord = 0 so parse_call
    // resolves the self-call there rather than to a create_function symbol). The birth is
    // genesis, so the root needs no migration/history chain. Reproducing the root exercises the
    // new object kind, the member nonce, the recursion_groups array, and the self-call
    // resolution. Covers a bare self-call, a base case, the self-call in arithmetic/let, two
    // self-calls, a bool result, and distinct names. Multi-member (mutual) recursion — which
    // needs the canonical member ordering — is the next increment.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        "fn f() -> i64 = f()\n",
        "fn fib() -> i64 = fib()\n",
        "fn f() -> i64 = if 1 < 2 then 1 else f()\n",
        "fn f() -> i64 = f() + 1\n",
        "fn f() -> i64 = f() + f()\n",
        "fn f() -> i64 = let x: i64 = f() in x + 1\n",
        "fn g() -> bool = if true then true else g()\n",
        "fn count() -> i64 = if 0 < 1 then count() else 0\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-rec-{i}.sqlite"));
        let src = temp.path().join(format!("ref-rec-{i}.cdb"));
        std::fs::write(&src, source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer self-recursion root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_mutual_recursion() {
    // 15a.4 (axis 2): MUTUAL RECURSION — two functions that call each other are a TWO-member
    // recursion group (create_recursion_group, both members born in one migration). This extends
    // the single-member (self-recursion) case with the genuinely new mechanics: (1) the canonical
    // MEMBER ORDERING — within a single SCC the members are ordered alphabetically by name, which
    // sets the `recursion_group:<ordinal>` nonces (member 0 = the alphabetically-first name); and
    // (2) PER-CALLEE ordinal resolution — a member's body calling the OTHER member resolves to that
    // member's distinct ordinal (not the caller's), so member 0's body references the
    // recursion_group:1 symbol and vice versa. The two-member RecursionGroup object lists both
    // members, and the ProgramRoot's names/param_names/symbols/recursion_groups arrays all carry
    // both. Both births are genesis (a single create_recursion_group migration), so the root needs
    // no migration/history chain — it is reproduced from objects alone. Covers source order
    // matching and contradicting canonical order, longer/bool/digit-bearing names, richer bodies
    // (arithmetic / `let` / `if`), a body with both a self-call and a cross-member call, and both
    // members having non-trivial bodies.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        "fn a() -> i64 = b()\nfn b() -> i64 = a()\n",
        "fn b() -> i64 = a()\nfn a() -> i64 = b()\n",
        "fn z() -> i64 = a()\nfn a() -> i64 = z()\n",
        "fn a() -> i64 = z()\nfn z() -> i64 = a()\n",
        "fn even() -> bool = odd()\nfn odd() -> bool = even()\n",
        "fn odd() -> bool = even()\nfn even() -> bool = odd()\n",
        "fn a() -> i64 = b() + 1\nfn b() -> i64 = a()\n",
        "fn a() -> i64 = b()\nfn b() -> i64 = a() + a()\n",
        "fn a() -> i64 = if true then a() else b()\nfn b() -> i64 = a()\n",
        "fn a() -> i64 = if false then b() else a()\nfn b() -> i64 = a()\n",
        "fn ping() -> i64 = let x: i64 = pong() in x\nfn pong() -> i64 = ping()\n",
        "fn p() -> i64 = q() * 2 + 3\nfn q() -> i64 = p()\n",
        "fn foo() -> i64 = bar()\nfn bar() -> i64 = foo()\n",
        "fn loop1() -> bool = loop2()\nfn loop2() -> bool = loop1()\n",
        "fn a() -> i64 = if 1 < 2 then b() else a()\nfn b() -> i64 = if 3 < 4 then a() else b()\n",
        // ASYMMETRIC cliques: the members differ structurally, so the canonical member order is
        // the WL round-2 colour refinement — NOT alphabetical and NOT the round-1 colour. Each of
        // these orders OPPOSITE to alphabetical (verified against the oracle), exercising round 2.
        "fn a() -> i64 = b()\nfn b() -> i64 = a() + 1\n",
        "fn a() -> i64 = b() + 1\nfn b() -> i64 = a()\n",
        "fn a() -> i64 = b() * 2\nfn b() -> i64 = a()\n",
        "fn a() -> i64 = b()\nfn b() -> i64 = let x: i64 = a() in x + 1\n",
        "fn a() -> i64 = b() + 1\nfn b() -> i64 = if true then a() else b()\n",
        "fn p() -> i64 = q()\nfn q() -> i64 = p() * 3 + 1\n",
        "fn foo() -> i64 = bar() + 2\nfn bar() -> i64 = foo()\n",
        "fn m() -> i64 = n() * 2\nfn n() -> i64 = m() + 1\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-mut-{i}.sqlite"));
        let src = temp.path().join(format!("ref-mut-{i}.cdb"));
        std::fs::write(&src, source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer mutual-recursion root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_call_arguments_and_params() {
    // 15a.4 (axis 2): function PARAMETERS in a multi-function program + call ARGUMENTS. Until now
    // every cross-symbol/recursive call was no-argument and every multi-function callee no-param.
    // This adds (1) parameters on a function in a TWO-function program — the callee's typed body
    // resolves param_refs, its FunctionSignature lists the parameter types, the ProgramRoot's
    // param_names carries the names (still symbol-hash-ordered, so they diverge from the
    // display-name-ordered names array — exercised by the toposort != alphabetical case), and the
    // create_function migration's operation/postcondition `params` array lists {name, type} pairs;
    // and (2) actual call arguments — the typed `call` node's `args` array holds the argument
    // expressions' hashes, parsed in the CALLER's scope (so an argument may reference the caller's
    // own parameters). Scope: i64/bool parameters (a sized-parameter argument would need the
    // callee's parameter type pushed down as the argument's expected type — deferred). Reproducing
    // the root exercises the parameter scope re-derivation, the signature/param_names/operation
    // parameter lists, and the argument list (single/multiple/nested/zero arguments, the call in
    // arithmetic / `if` / `let`, an argument using the caller's parameter, and the divergence case).
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // --- parameters in a multi-function program (independent functions, no calls) ---
        "fn helper(x: i64) -> i64 = x + 1\nfn main() -> i64 = 42\n",
        "fn main() -> i64 = 42\nfn helper(x: i64) -> i64 = x + 1\n", // reversed source order
        "fn f(x: i64) -> i64 = x\nfn g(y: i64) -> i64 = y\n",        // both have parameters
        "fn f(a: i64, b: i64) -> i64 = a + b\nfn main() -> i64 = 7\n", // canonical-first, two params
        "fn p(a: bool) -> bool = a\nfn q() -> bool = true\n",        // bool parameter
        "fn s(a: u8) -> u8 = a\nfn t() -> u8 = 1\n",                 // sized parameter (sig/op-params/param_ref u8)
        // --- call arguments (two-function DAG) ---
        "fn add(a: i64, b: i64) -> i64 = a + b\nfn main() -> i64 = add(2, 3)\n",
        "fn main() -> i64 = add(2, 3)\nfn add(a: i64, b: i64) -> i64 = a + b\n", // reversed source
        "fn one(a: i64) -> i64 = a\nfn main() -> i64 = one(99)\n",   // single argument
        "fn k(a: i64, b: i64, c: i64) -> i64 = a + b + c\nfn main() -> i64 = k(1, 2, 3)\n", // three args/params
        "fn k4(a: i64, b: i64, c: i64, d: i64) -> i64 = a + b + c + d\nfn main() -> i64 = k4(1, 2, 3, 4)\n", // four
        "fn wide(alpha: i64, beta: i64, gamma: i64) -> i64 = alpha + beta\nfn main() -> i64 = wide(7, 8, 9)\n", // longer names
        "fn k3(a: i64, b: i64, c: i64) -> i64 = a + b + c\nfn zz() -> i64 = 0\n", // 3-param canonical-first, uncalled
        "fn add(a: i64, b: i64) -> i64 = a + b\nfn main() -> i64 = add(add(1, 2), 3)\n",    // nested call arg
        "fn g(a: i64) -> i64 = a\nfn main(x: i64) -> i64 = g(x + 1)\n",      // arg uses caller's parameter
        "fn h(a: i64) -> i64 = a + a\nfn main(p: i64) -> i64 = h(p) + h(p)\n", // two calls, caller has param
        "fn add(a: i64, b: i64) -> i64 = a + b\nfn main() -> i64 = add(2, 3) + 1\n",        // call in arithmetic
        "fn dbl(a: i64) -> i64 = a + a\nfn main() -> i64 = if 1 < 2 then dbl(5) else 0\n",  // call in if
        "fn dbl(a: i64) -> i64 = a + a\nfn main() -> i64 = let y: i64 = dbl(7) in y + 1\n", // call in let
        "fn f(a: bool) -> bool = a\nfn main() -> bool = f(true)\n",  // bool parameter + bool argument
        "fn f(a: i64, b: i64) -> i64 = a * b + a\nfn main() -> i64 = f(3, 4)\n", // deeper param-using body
        // dependency order (callee `zzz` first) CONTRADICTS alphabetical, AND the callee has params,
        // so param_names (symbol-hash-ordered) diverges from names (display-ordered).
        "fn zzz(a: i64) -> i64 = a\nfn aaa() -> i64 = zzz(5)\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-arg-{i}.sqlite"));
        let src = temp.path().join(format!("ref-arg-{i}.cdb"));
        std::fs::write(&src, source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer call-arguments/params root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_recursion_group_params_and_args() {
    // 15a.4 (axis 2): PARAMETERS + ARGUMENTS in RECURSION GROUPS (self- and mutual-recursion).
    // A recursion-group member may now have parameters and its self/cross-member call may pass
    // arguments — extending the no-parameter recursion groups. The member's body resolves
    // param_refs and the recursive call carries args (reusing the call-argument machinery); the
    // signature/param_names carry the parameters; and — the genuinely new mechanic — the WL
    // member-ordering colour's static signature now includes the member's parameter TYPE NAMES
    // (src/lib.rs recursion_member_static_sigs), so member_color must emit them or the canonical
    // member order (and hence the recursion_group:<ordinal> nonces and the symbols) can diverge.
    // Because both births are genesis, the recursion-group root is reproduced from objects alone
    // (no migration). Covers self-recursion (one/two parameters, i64/bool results) and mutual
    // recursion (both source orders, asymmetric cliques whose WL order is opposite alphabetical,
    // i64 and bool members) — all with parameters and argument-passing recursive calls.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // self-recursion with parameters (one-member group; no member ordering)
        "fn fact(n: i64) -> i64 = if n == 0 then 1 else n * fact(n - 1)\n",
        "fn f(n: i64) -> i64 = if n == 0 then 0 else f(n - 1)\n",
        "fn sum(a: i64, b: i64) -> i64 = if a == 0 then b else sum(a - 1, b + 1)\n",
        "fn g(n: i64) -> bool = if n == 0 then true else g(n - 1)\n",
        // mutual recursion with parameters (two-member group; static_sig now carries param types)
        "fn is_even(n: i64) -> bool = if n == 0 then true else is_odd(n - 1)\nfn is_odd(n: i64) -> bool = if n == 0 then false else is_even(n - 1)\n",
        "fn is_odd(n: i64) -> bool = if n == 0 then false else is_even(n - 1)\nfn is_even(n: i64) -> bool = if n == 0 then true else is_odd(n - 1)\n",
        "fn ping(n: i64) -> i64 = pong(n - 1)\nfn pong(n: i64) -> i64 = ping(n - 1)\n",
        "fn a(x: i64) -> i64 = b(x) + 1\nfn b(x: i64) -> i64 = a(x)\n",
        "fn ev(n: i64) -> i64 = if n == 0 then 1 else od(n - 1)\nfn od(n: i64) -> i64 = if n == 0 then 0 else ev(n - 1)\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-recp-{i}.sqlite"));
        let src = temp.path().join(format!("ref-recp-{i}.cdb"));
        std::fs::write(&src, source).unwrap();
        run(&["init", path(&db)]);
        let report = run(&["import", path(&db), path(&src)]);
        let want = report
            .lines()
            .find_map(|line| line.strip_prefix("root "))
            .expect("import reports a root");
        let got = run_hasher(exe, source.as_bytes());
        assert_eq!(
            got,
            want,
            "self-hosted importer recursion-group params/args root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn the_committed_lexer_view_passes_the_checked_view_gate() {
    // SPEC_V3 §11: the committed .cdb is a checked view. The lexer's build is a
    // two-import bootstrap (std/fmt.cdb + compiler/front/lex.cdb), so the gate
    // goes through one consolidation: import the committed sources, export the
    // canonical projection, re-import it — the re-exported projection must be
    // byte-stable and the root a fixpoint (mirrors the evaluator's gate).
    let temp = tempdir().unwrap();
    let db1 = temp.path().join("view1.sqlite");
    run(&["init", path(&db1)]);
    run(&["import", path(&db1), "std/fmt.cdb"]);
    run(&["import", path(&db1), "compiler/front/lex.cdb"]);
    run(&["verify", path(&db1)]);
    let export1 = temp.path().join("view1.cdb");
    run(&["export", path(&db1), "--branch", "main", "--out", path(&export1)]);

    let db2 = temp.path().join("view2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export1)]);
    run(&["verify", path(&db2)]);
    let export2 = temp.path().join("view2.cdb");
    run(&["export", path(&db2), "--branch", "main", "--out", path(&export2)]);

    let db3 = temp.path().join("view3.sqlite");
    run(&["init", path(&db3)]);
    run(&["import", path(&db3), path(&export2)]);
    let export3 = temp.path().join("view3.cdb");
    run(&["export", path(&db3), "--branch", "main", "--out", path(&export3)]);

    // One consolidation reaches the canonical fixpoint: byte-stable projection
    // and a reproduced root hash.
    let text2 = std::fs::read(&export2).unwrap();
    let text3 = std::fs::read(&export3).unwrap();
    assert_eq!(text2, text3, "canonical projection is byte-stable");
    let root = |db: &Path| {
        let history = run(&["history", path(db), "--json"]);
        serde_json::from_str::<serde_json::Value>(&history).unwrap()["root_hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(root(&db2), root(&db3), "root hash is a fixpoint");
}

#[test]
fn emit_objects_is_a_deterministic_canonical_dump() {
    // The 15a.0 importer-oracle substrate: emit-objects dumps every object's
    // (hash, kind, schema_version, canonical payload) plus the root. It must be
    // byte-identical across an independent rebuild from the same source —
    // the deterministic-birth-identity property (SPEC_V3 §10) the self-hosted
    // importer's root-hash oracle will rest on.
    let temp = tempdir().unwrap();
    let src = temp.path().join("min.cdb");
    std::fs::write(&src, "fn main() -> i64 = 1 + 2\n").unwrap();

    let dump = |db_name: &str| {
        let db = temp.path().join(db_name);
        run(&["init", path(&db)]);
        run(&["import", path(&db), path(&src)]);
        let out = temp.path().join(format!("{db_name}.objects"));
        run(&["emit-objects", path(&db), "--out", path(&out)]);
        std::fs::read_to_string(&out).unwrap()
    };

    let first = dump("a.sqlite");
    let second = dump("b.sqlite");
    assert_eq!(
        first, second,
        "emit-objects must be byte-identical across an independent rebuild"
    );

    // Structure: a trailing `root sha256:…` pin, and the program's core objects
    // named with their canonical (sorted-key) payloads.
    let root_line = first.lines().last().expect("a root line");
    assert!(
        root_line.starts_with("root sha256:"),
        "dump ends with the root pin, got: {root_line}"
    );
    assert!(
        first.contains("\tFunctionDef\t"),
        "dump names the FunctionDef object"
    );
    assert!(
        first.contains("\tProgramRoot\t"),
        "dump names the ProgramRoot object"
    );
    // The importer's reported root must equal the dump's root pin.
    let c_db = temp.path().join("c.sqlite");
    run(&["init", path(&c_db)]);
    let import_report = run(&["import", path(&c_db), path(&src)]);
    let new_root = import_report
        .lines()
        .find_map(|line| line.strip_prefix("root "))
        .expect("import reports a root");
    assert_eq!(
        root_line,
        format!("root {new_root}"),
        "emit-objects root pin equals the importer's reported root"
    );
}
