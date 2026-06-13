// Phase 15 (ladder rung A): the self-hosted front-end, stage 15a.1.
//
// `compiler/front/lex.cdb` is imported, verified, built to a NATIVE binary,
// and fed source bytes on stdin. It tokenizes the source exactly like the Rust
// lexer (src/expr.rs `lex`) and prints the token-stream PROBE
// `tokens <count> fnv32 <digest>`. That probe must equal the Rust reference
// `codedb::token_probe` on the same source — the determinism oracle for this
// first front-end increment (docs/PLAN_V3.md Phase 15a; the lexer counterpart
// of the Phase 8 loader probe).
//
// This stage's corpus is ASCII and free of string / byte-string literals (a
// token's text is then a direct source slice, so the byte machine matches the
// Rust lexer's `char` walk exactly); string-literal lexing is a documented
// follow-on. The 15a.0 substrate (`emit-objects`, the importer oracle the next
// sub-stage will check the .cdb importer's objects + root hash against) is also
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
}

#[test]
fn lexer_probe_matches_rust_on_real_string_free_sources() {
    if !can_build_default_native_target() {
        return;
    }
    let exe = lexer();
    // Real committed CodeDB sources (no string/byte-string literals): the
    // self-hosted lexer tokenizes them identically to the Rust lexer — dogfood.
    for file in [
        "std/core.cdb",
        "std/mem.cdb",
        "std/result.cdb",
        "std/alloc.cdb",
    ] {
        let source = std::fs::read_to_string(file).unwrap_or_else(|_| panic!("read {file}"));
        assert_probe(exe, &source);
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
