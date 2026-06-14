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
//   compiler/front/import.cdb  reads `fn main() -> i64 = <int>`, builds the six
//                              canonical objects + chains their hashes, and emits a
//                              ProgramRoot hash == the Rust importer's root for the
//                              same source (rung-A importer milestone, minimal grammar).
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
