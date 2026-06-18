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

/// Import + verify + build the `esc` entry of compiler/front/json.cdb (the
/// canonical JSON string writer) — once per test process. lib.cdb supplies
/// read_all and the byte plumbing; json.cdb adds the measure/emit escaper.
fn json_escaper() -> &'static Path {
    static JSON_ESCAPER: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    JSON_ESCAPER
        .get_or_init(|| {
            let temp = tempdir().unwrap();
            let db = temp.path().join("selfhost-json.sqlite");
            run(&["init", path(&db)]);
            run(&["import", path(&db), "compiler/front/lib.cdb"]);
            run(&["import", path(&db), "compiler/front/json.cdb"]);
            run(&["verify", path(&db)]);
            let exe = temp.path().join("esc-bin");
            run(&["build", path(&db), "esc", "--out", path(&exe)]);
            (temp, exe)
        })
        .1
        .as_path()
}

/// Run the `esc` binary with `bytes` on stdin; return its raw stdout bytes.
fn run_esc(exe: &Path, bytes: &[u8]) -> Vec<u8> {
    let mut child = StdCommand::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn esc");
    child
        .stdin
        .take()
        .expect("esc stdin")
        .write_all(bytes)
        .expect("write esc stdin");
    let output = child.wait_with_output().expect("wait esc");
    assert!(output.status.success(), "esc exited non-zero: {output:?}");
    output.stdout
}

/// Assert the .cdb escaper's output equals codedb's canonical JSON string
/// encoding. canonical_json (src/store.rs:1164) serializes a String via
/// serde_json::to_string, so that call IS the authoritative oracle.
fn assert_esc(exe: &Path, s: &str) {
    let got = run_esc(exe, s.as_bytes());
    let want = serde_json::to_string(s).expect("serde canonical string");
    assert_eq!(
        got.as_slice(),
        want.as_bytes(),
        "json.cdb escaping mismatch for {s:?} (got {got:?}, want {want:?})"
    );
}

#[test]
fn json_escaper_matches_canonical_json_string_encoding() {
    // json.cdb's measure/emit string writer must reproduce codedb's canonical
    // JSON string encoding (serde_json::to_string) byte for byte. The importer
    // corpus is escape-free, so this is the ONLY gate that exercises escaping;
    // it also proves the exactly-sized string_with_capacity(measure) buffer
    // never under-shoots (e.g. the all-escape runs below fill it to capacity).
    if !can_build_default_native_target() {
        return;
    }
    let exe = json_escaper();
    // Escape-free values the importer actually emits (idents, hashes, operators,
    // type names, nonces): the common path, unchanged by escaping.
    assert_esc(exe, "");
    assert_esc(exe, "main");
    assert_esc(exe, "sha256:5aa0031cafebabedeadbeef0123456789");
    assert_esc(exe, "i64");
    assert_esc(exe, "import:main:foo:0");
    assert_esc(exe, "recursion_group:0");
    assert_esc(exe, "<<");
    assert_esc(exe, "&&");
    assert_esc(exe, "/"); // '/' is NOT escaped by serde
    // The seven named escapes.
    assert_esc(exe, "\"");
    assert_esc(exe, "\\");
    assert_esc(exe, "\u{8}");
    assert_esc(exe, "\t");
    assert_esc(exe, "\n");
    assert_esc(exe, "\u{c}");
    assert_esc(exe, "\r");
    // Other control bytes -> \u00xx (lowercase); DEL (0x7f) stays raw.
    assert_esc(exe, "\u{0}");
    assert_esc(exe, "\u{1}");
    assert_esc(exe, "\u{1f}");
    assert_esc(exe, "\u{7f}");
    // Mixed, embedded NUL, and long all-escape / escape-free runs (buffer sizing).
    assert_esc(exe, "tab\there\nquote\" back\\slash");
    assert_esc(exe, "a\u{0}b\u{1}c");
    assert_esc(exe, "\"\\\u{8}\t\n\u{c}\r\u{0}\u{1f}\u{7f}");
    assert_esc(exe, &"x".repeat(200));
    assert_esc(exe, &"\t".repeat(64));
    // Non-ASCII passes through as raw UTF-8 bytes (byte machine, byte-faithful).
    assert_esc(exe, "café");
    assert_esc(exe, "smørrebrød");
}

/// Import + verify + build json.cdb's scalar-leaf gate entries (jint/jbool/jnull)
/// — once per test process.
fn json_scalar_bins() -> &'static (TempDir, PathBuf, PathBuf, PathBuf) {
    static JSON_SCALARS: OnceLock<(TempDir, PathBuf, PathBuf, PathBuf)> = OnceLock::new();
    JSON_SCALARS.get_or_init(|| {
        let temp = tempdir().unwrap();
        let db = temp.path().join("selfhost-json-scalars.sqlite");
        run(&["init", path(&db)]);
        run(&["import", path(&db), "compiler/front/lib.cdb"]);
        run(&["import", path(&db), "compiler/front/json.cdb"]);
        run(&["verify", path(&db)]);
        let jint = temp.path().join("jint-bin");
        let jbool = temp.path().join("jbool-bin");
        let jnull = temp.path().join("jnull-bin");
        run(&["build", path(&db), "jint", "--out", path(&jint)]);
        run(&["build", path(&db), "jbool", "--out", path(&jbool)]);
        run(&["build", path(&db), "jnull", "--out", path(&jnull)]);
        (temp, jint, jbool, jnull)
    })
}

#[test]
fn json_scalars_match_canonical_json() {
    // json.cdb's bare-integer / bool / null leaves must reproduce canonical_json's
    // rendering (serde_json::to_string of the value): a Number bare, a Bool as
    // true/false, null as null. json_int is scoped non-negative (the only bare-int
    // domain the importer emits); jint parses stdin then emits, so the exactly-sized
    // string_with_capacity(json_int_len) is exercised across digit-count boundaries.
    if !can_build_default_native_target() {
        return;
    }
    let bins = json_scalar_bins();
    let (jint, jbool, jnull) = (&bins.1, &bins.2, &bins.3);
    for s in [
        "0", "1", "9", "10", "11", "42", "99", "100", "101", "999", "1000",
        "1000000", "123456789", "9999999999", "9223372036854775807",
    ] {
        let n: i64 = s.parse().unwrap();
        let got = run_esc(jint, s.as_bytes());
        assert_eq!(
            got.as_slice(),
            serde_json::to_string(&n).unwrap().as_bytes(),
            "jint mismatch for {s}"
        );
    }
    assert_eq!(
        run_esc(jbool, b"x").as_slice(),
        serde_json::to_string(&true).unwrap().as_bytes()
    );
    assert_eq!(
        run_esc(jbool, b"").as_slice(),
        serde_json::to_string(&false).unwrap().as_bytes()
    );
    assert_eq!(
        run_esc(jnull, b"").as_slice(),
        serde_json::to_string(&()).unwrap().as_bytes()
    );
}

/// Import + verify + build json.cdb's `jobj` composite-object gate entry.
fn json_object_bin() -> &'static Path {
    static JSON_OBJECT: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    JSON_OBJECT
        .get_or_init(|| {
            let temp = tempdir().unwrap();
            let db = temp.path().join("selfhost-json-object.sqlite");
            run(&["init", path(&db)]);
            run(&["import", path(&db), "compiler/front/lib.cdb"]);
            run(&["import", path(&db), "compiler/front/json.cdb"]);
            run(&["verify", path(&db)]);
            let exe = temp.path().join("jobj-bin");
            run(&["build", path(&db), "jobj", "--out", path(&exe)]);
            (temp, exe)
        })
        .1
        .as_path()
}

#[test]
fn json_object_framing_matches_canonical_json() {
    // A 2-field object {"depth":int,"name":string} built through json.cdb's push_lit
    // skeleton + json_int + json_str with an EXACT measure must equal canonical_json
    // (keys byte-sorted: depth < name). The .cdb sets depth = string_len(stdin) and
    // name = stdin, so this exercises the composite measure across escaped / UTF-8 /
    // control-byte names and proves the skeleton-plus-leaves discipline.
    if !can_build_default_native_target() {
        return;
    }
    let exe = json_object_bin();
    let big = "x".repeat(50);
    let names: [&str; 10] = [
        "", "hi", "main", "café", big.as_str(), "a\"b", "tab\there",
        "a\nb\\c", "sha256:deadbeef", "\u{0}\u{1}\u{1f}",
    ];
    for name in names {
        let depth = name.len(); // byte length == string_len(stdin)
        let want = format!(
            "{{\"depth\":{},\"name\":{}}}",
            depth,
            serde_json::to_string(name).unwrap()
        );
        let got = run_esc(exe, name.as_bytes());
        assert_eq!(got.as_slice(), want.as_bytes(), "jobj mismatch for {name:?}");
    }
}

/// Import + verify + build object.cdb's `tobj` Type-object gate entry
/// (lib.cdb + json.cdb + object.cdb) — once per test process.
fn type_object_builder() -> &'static Path {
    static TYPE_OBJ: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    TYPE_OBJ
        .get_or_init(|| {
            let temp = tempdir().unwrap();
            let db = temp.path().join("selfhost-object.sqlite");
            run(&["init", path(&db)]);
            run(&["import", path(&db), "compiler/front/lib.cdb"]);
            run(&["import", path(&db), "compiler/front/json.cdb"]);
            run(&["import", path(&db), "compiler/front/object.cdb"]);
            run(&["verify", path(&db)]);
            let exe = temp.path().join("tobj-bin");
            run(&["build", path(&db), "tobj", "--out", path(&exe)]);
            (temp, exe)
        })
        .1
        .as_path()
}

#[test]
fn object_build_type_matches_emit_objects() {
    // object.cdb's build_type must reproduce the REAL Type object hashes CodeDB
    // assigns — built through json.cdb's measure/emit writer + an exactly-sized
    // hash_object preimage. Oracle: emit-objects on a program using all 9 scalar
    // types, mapping each {"type_kind":...} payload to its content hash. The .cdb
    // sets tyc = string_len(stdin), so feeding 0..8 bytes selects
    // i64/bool/u8/u16/u32/u64/i8/i16/i32.
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let db = temp.path().join("types.sqlite");
    let src = temp.path().join("types.cdb");
    std::fs::write(
        &src,
        "fn f(a: i64, b: bool, c: u8, d: u16, e: u32, g: u64, h: i8, i: i16, j: i32) -> i64 = a\n\
         fn main() -> i64 = 0\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let dump_path = temp.path().join("dump.txt");
    run(&["emit-objects", path(&db), "--out", path(&dump_path)]);
    let dump = std::fs::read_to_string(&dump_path).unwrap();
    let mut payload_to_hash: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for line in dump.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() == 4 && cols[1] == "Type" {
            payload_to_hash.insert(cols[3].to_string(), cols[0].to_string());
        }
    }

    let exe = type_object_builder();
    let kinds = [
        (0usize, "I64"), (1, "Bool"), (2, "U8"), (3, "U16"), (4, "U32"),
        (5, "U64"), (6, "I8"), (7, "I16"), (8, "I32"),
    ];
    for (tyc, kind) in kinds {
        let payload = format!("{{\"type_kind\":\"{kind}\"}}");
        let want = payload_to_hash
            .get(&payload)
            .unwrap_or_else(|| panic!("emit-objects had no Type {payload}"));
        let got = run_esc(exe, &vec![b'x'; tyc]);
        assert_eq!(
            got.as_slice(),
            want.as_bytes(),
            "build_type({tyc}) {kind} mismatch"
        );
    }
}

/// Import + verify + build object.cdb's SymbolBirth gate entries: sbfn/sbty (3-key
/// function/type) and sbrf/sbev (4-key owned record_field/enum_variant).
fn symbol_birth_bins() -> &'static (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    static SB: OnceLock<(TempDir, PathBuf, PathBuf, PathBuf, PathBuf)> = OnceLock::new();
    SB.get_or_init(|| {
        let temp = tempdir().unwrap();
        let db = temp.path().join("selfhost-symbolbirth.sqlite");
        run(&["init", path(&db)]);
        run(&["import", path(&db), "compiler/front/lib.cdb"]);
        run(&["import", path(&db), "compiler/front/json.cdb"]);
        run(&["import", path(&db), "compiler/front/object.cdb"]);
        run(&["verify", path(&db)]);
        let sbfn = temp.path().join("sbfn-bin");
        let sbty = temp.path().join("sbty-bin");
        let sbrf = temp.path().join("sbrf-bin");
        let sbev = temp.path().join("sbev-bin");
        run(&["build", path(&db), "sbfn", "--out", path(&sbfn)]);
        run(&["build", path(&db), "sbty", "--out", path(&sbty)]);
        run(&["build", path(&db), "sbrf", "--out", path(&sbrf)]);
        run(&["build", path(&db), "sbev", "--out", path(&sbev)]);
        (temp, sbfn, sbty, sbrf, sbev)
    })
}

#[test]
fn object_build_symbol_birth_matches_emit_objects() {
    // object.cdb's SymbolBirth builders must reproduce the real hashes. Oracle:
    // emit-objects on a program with functions, types, a record, and an enum; for
    // each SymbolBirth, feed its fields to the matching entry (3-key function/type
    // via sbfn/sbty; 4-key owned record_field/enum_variant via sbrf/sbev) and diff
    // the hash. Covers bh = "genesis" and bh = a history hash.
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let db = temp.path().join("prog.sqlite");
    let src = temp.path().join("prog.cdb");
    std::fs::write(
        &src,
        "record R { x: i64  y: i64 }\n\
         enum E { a: i64  b: bool }\n\
         fn helper() -> i64 = 1\n\
         fn main() -> i64 = helper()\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let dump_path = temp.path().join("dump.txt");
    run(&["emit-objects", path(&db), "--out", path(&dump_path)]);
    let dump = std::fs::read_to_string(&dump_path).unwrap();

    let bins = symbol_birth_bins();
    let (sbfn, sbty, sbrf, sbev) = (&bins.1, &bins.2, &bins.3, &bins.4);
    let mut checked = 0usize;
    for line in dump.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() != 4 || cols[1] != "SymbolBirth" {
            continue;
        }
        let (hash, payload) = (cols[0], cols[3]);
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        let bh = v["birth_history_hash"].as_str().unwrap();
        let nonce = v["local_nonce"].as_str().unwrap();
        let kind = v["symbol_kind"].as_str().unwrap();
        let got = if let Some(owner) = v.get("owner_type_symbol") {
            // 4-key owned form: record_field / enum_variant.
            let owner = owner.as_str().unwrap();
            let exe = if kind == "enum_variant" { sbev } else { sbrf };
            run_esc(exe, format!("{bh}\n{nonce}\n{owner}").as_bytes())
        } else {
            // 3-key form: function / type.
            let exe = if kind == "function" { sbfn } else { sbty };
            run_esc(exe, format!("{bh}\n{nonce}").as_bytes())
        };
        assert_eq!(
            got.as_slice(),
            hash.as_bytes(),
            "SymbolBirth mismatch for {kind} {nonce}"
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "expected several SymbolBirths (3-key + owned); checked {checked}"
    );
}

/// Import + verify + build object.cdb's `sigobj` FunctionSignature gate entry.
fn signature_builder() -> &'static Path {
    static SIG: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    SIG.get_or_init(|| {
        let temp = tempdir().unwrap();
        let db = temp.path().join("selfhost-signature.sqlite");
        run(&["init", path(&db)]);
        run(&["import", path(&db), "compiler/front/lib.cdb"]);
        run(&["import", path(&db), "compiler/front/json.cdb"]);
        run(&["import", path(&db), "compiler/front/object.cdb"]);
        run(&["verify", path(&db)]);
        let exe = temp.path().join("sigobj-bin");
        run(&["build", path(&db), "sigobj", "--out", path(&exe)]);
        (temp, exe)
    })
    .1
    .as_path()
}

#[test]
fn object_build_signature_matches_emit_objects() {
    // object.cdb's build_signature — the first array kind (params = a JSON array of
    // parameter Type hashes) — must reproduce the real FunctionSignature hashes.
    // Oracle: emit-objects a program of various-arity functions; map each Type hash
    // to its code via the Type objects in the SAME dump, then for each
    // FunctionSignature feed [return_code, param_codes...] to sigobj and diff.
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let db = temp.path().join("prog.sqlite");
    let src = temp.path().join("prog.cdb");
    std::fs::write(
        &src,
        "fn f0() -> i64 = 0\n\
         fn f1(a: i64) -> i64 = a\n\
         fn f2(a: i64, b: bool) -> bool = b\n\
         fn f3(a: u8, b: u16, c: u32) -> u64 = 0\n\
         fn main() -> i64 = 0\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let dump_path = temp.path().join("dump.txt");
    run(&["emit-objects", path(&db), "--out", path(&dump_path)]);
    let dump = std::fs::read_to_string(&dump_path).unwrap();

    let kind_code = |k: &str| -> u8 {
        match k {
            "I64" => 0, "Bool" => 1, "U8" => 2, "U16" => 3, "U32" => 4,
            "U64" => 5, "I8" => 6, "I16" => 7, "I32" => 8,
            other => panic!("unexpected type_kind {other}"),
        }
    };
    let mut hash_to_code: std::collections::BTreeMap<String, u8> =
        std::collections::BTreeMap::new();
    for line in dump.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() == 4 && cols[1] == "Type" {
            let v: serde_json::Value = serde_json::from_str(cols[3]).unwrap();
            hash_to_code.insert(cols[0].to_string(), kind_code(v["type_kind"].as_str().unwrap()));
        }
    }

    let exe = signature_builder();
    let mut checked = 0usize;
    for line in dump.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() != 4 || cols[1] != "FunctionSignature" {
            continue;
        }
        let (hash, payload) = (cols[0], cols[3]);
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        let mut input: Vec<u8> = vec![hash_to_code[v["return"].as_str().unwrap()]];
        for p in v["params"].as_array().unwrap() {
            input.push(hash_to_code[p.as_str().unwrap()]);
        }
        let got = run_esc(exe, &input);
        assert_eq!(
            got.as_slice(),
            hash.as_bytes(),
            "FunctionSignature mismatch for {payload}"
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "expected several FunctionSignatures; checked {checked}"
    );
}

/// Import + verify + build object.cdb's `fdobj` FunctionDef gate entry.
fn funcdef_builder() -> &'static Path {
    static FD: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    FD.get_or_init(|| {
        let temp = tempdir().unwrap();
        let db = temp.path().join("selfhost-funcdef.sqlite");
        run(&["init", path(&db)]);
        run(&["import", path(&db), "compiler/front/lib.cdb"]);
        run(&["import", path(&db), "compiler/front/json.cdb"]);
        run(&["import", path(&db), "compiler/front/object.cdb"]);
        run(&["verify", path(&db)]);
        let exe = temp.path().join("fdobj-bin");
        run(&["build", path(&db), "fdobj", "--out", path(&exe)]);
        (temp, exe)
    })
    .1
    .as_path()
}

#[test]
fn object_build_funcdef_matches_emit_objects() {
    // object.cdb's build_funcdef frames a FunctionDef from its three referenced
    // child hashes (signature, symbol, body Expression). Oracle: emit-objects a
    // two-function program; for each FunctionDef feed "sig\nsym\nbody" to fdobj and
    // diff the hash.
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let db = temp.path().join("prog.sqlite");
    let src = temp.path().join("prog.cdb");
    std::fs::write(
        &src,
        "fn helper(a: i64) -> i64 = a + 1\n\
         fn main() -> i64 = helper(41)\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let dump_path = temp.path().join("dump.txt");
    run(&["emit-objects", path(&db), "--out", path(&dump_path)]);
    let dump = std::fs::read_to_string(&dump_path).unwrap();

    let exe = funcdef_builder();
    let mut checked = 0usize;
    for line in dump.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() != 4 || cols[1] != "FunctionDef" {
            continue;
        }
        let (hash, payload) = (cols[0], cols[3]);
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        let sig = v["function_sig_hash"].as_str().unwrap();
        let sym = v["symbol"].as_str().unwrap();
        let body = v["typed_body_expr_hash"].as_str().unwrap();
        let got = run_esc(exe, format!("{sig}\n{sym}\n{body}").as_bytes());
        assert_eq!(
            got.as_slice(),
            hash.as_bytes(),
            "FunctionDef mismatch for {payload}"
        );
        checked += 1;
    }
    assert!(checked >= 2, "expected several FunctionDefs; checked {checked}");
}

/// Import + verify + build object.cdb's Expression gate entries: exlit/exbool/exbin
/// (literal_i64/literal_bool/binary) and exprm/exlr/exun/exif/exic (param_ref/
/// local_ref/unary/if/int_cast).
type ExprBins = (
    TempDir, PathBuf, PathBuf, PathBuf, PathBuf, PathBuf, PathBuf, PathBuf, PathBuf, PathBuf,
    PathBuf,
);
fn expression_bins() -> &'static ExprBins {
    static EX: OnceLock<ExprBins> = OnceLock::new();
    EX.get_or_init(|| {
        let temp = tempdir().unwrap();
        let db = temp.path().join("selfhost-expression.sqlite");
        run(&["init", path(&db)]);
        run(&["import", path(&db), "compiler/front/lib.cdb"]);
        run(&["import", path(&db), "compiler/front/json.cdb"]);
        run(&["import", path(&db), "compiler/front/object.cdb"]);
        run(&["verify", path(&db)]);
        let bin = |name: &str| -> PathBuf {
            let exe = temp.path().join(format!("{name}-bin"));
            run(&["build", path(&db), name, "--out", path(&exe)]);
            exe
        };
        let (exlit, exbool, exbin) = (bin("exlit"), bin("exbool"), bin("exbin"));
        let (exprm, exlr, exun) = (bin("exprm"), bin("exlr"), bin("exun"));
        let (exif, exic, exlet) = (bin("exif"), bin("exic"), bin("exlet"));
        let excall = bin("excall");
        (temp, exlit, exbool, exbin, exprm, exlr, exun, exif, exic, exlet, excall)
    })
}

#[test]
fn object_build_expression_matches_emit_objects() {
    // object.cdb's Expression builders (this increment: literal_i64, literal_bool,
    // binary) must reproduce the real Expression hashes. Oracle: emit-objects a
    // program with int/bool literals and binary expressions; map Type hashes to
    // codes via the Type objects in the dump, then for each Expression of a covered
    // kind feed its fields to the matching entry and diff. Other kinds (param_ref,
    // ...) are skipped (later increments). A binary's child hashes come straight
    // from its payload, so nested binaries reproduce without rebuilding children.
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let db = temp.path().join("prog.sqlite");
    let src = temp.path().join("prog.cdb");
    std::fs::write(
        &src,
        "fn f(a: i64) -> i64 = -a\n\
         fn g(a: i64) -> i64 = if a < 0 then 1 else a\n\
         fn h(a: i64) -> i64 = let x: i64 = a + 1 in x + a\n\
         fn c(a: i64) -> u8 = to_u8(a)\n\
         fn t() -> bool = true\n\
         fn caller() -> i64 = h(7)\n\
         fn main() -> i64 = 1 + 2\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let dump_path = temp.path().join("dump.txt");
    run(&["emit-objects", path(&db), "--out", path(&dump_path)]);
    let dump = std::fs::read_to_string(&dump_path).unwrap();

    let kind_code = |k: &str| -> u8 {
        match k {
            "I64" => 0, "Bool" => 1, "U8" => 2, "U16" => 3, "U32" => 4,
            "U64" => 5, "I8" => 6, "I16" => 7, "I32" => 8,
            other => panic!("unexpected type_kind {other}"),
        }
    };
    let mut hash_to_code: std::collections::BTreeMap<String, u8> =
        std::collections::BTreeMap::new();
    for line in dump.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() == 4 && cols[1] == "Type" {
            let v: serde_json::Value = serde_json::from_str(cols[3]).unwrap();
            hash_to_code.insert(cols[0].to_string(), kind_code(v["type_kind"].as_str().unwrap()));
        }
    }

    let bins = expression_bins();
    let (exlit, exbool, exbin) = (&bins.1, &bins.2, &bins.3);
    let (exprm, exlr, exun, exif, exic) = (&bins.4, &bins.5, &bins.6, &bins.7, &bins.8);
    let (exlet, excall) = (&bins.9, &bins.10);
    let mut checked = 0usize;
    for line in dump.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() != 4 || cols[1] != "Expression" {
            continue;
        }
        let (hash, payload) = (cols[0], cols[3]);
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        let got = match v["expr_kind"].as_str().unwrap() {
            "literal_i64" => {
                let mut input = vec![hash_to_code[v["type"].as_str().unwrap()]];
                input.extend_from_slice(v["value"].as_str().unwrap().as_bytes());
                run_esc(exlit, &input)
            }
            "literal_bool" => {
                let b = v["value"].as_bool().unwrap();
                run_esc(exbool, (if b { "x" } else { "" }).as_bytes())
            }
            "binary" => {
                let s = format!(
                    "{}\n{}\n{}\n{}",
                    v["left"].as_str().unwrap(),
                    v["op"].as_str().unwrap(),
                    v["right"].as_str().unwrap(),
                    v["type"].as_str().unwrap()
                );
                run_esc(exbin, s.as_bytes())
            }
            "param_ref" => {
                let mut input = vec![v["index"].as_u64().unwrap() as u8];
                input.extend_from_slice(v["type"].as_str().unwrap().as_bytes());
                run_esc(exprm, &input)
            }
            "local_ref" => {
                let mut input = vec![v["depth"].as_u64().unwrap() as u8];
                input.extend_from_slice(v["type"].as_str().unwrap().as_bytes());
                run_esc(exlr, &input)
            }
            "unary" => {
                let s = format!(
                    "{}\n{}\n{}",
                    v["expr"].as_str().unwrap(),
                    v["op"].as_str().unwrap(),
                    v["type"].as_str().unwrap()
                );
                run_esc(exun, s.as_bytes())
            }
            "if" => {
                let s = format!(
                    "{}\n{}\n{}\n{}",
                    v["cond"].as_str().unwrap(),
                    v["else"].as_str().unwrap(),
                    v["then"].as_str().unwrap(),
                    v["type"].as_str().unwrap()
                );
                run_esc(exif, s.as_bytes())
            }
            "int_cast" => {
                let s = format!(
                    "{}\n{}\n{}",
                    v["source_type"].as_str().unwrap(),
                    v["type"].as_str().unwrap(),
                    v["value"].as_str().unwrap()
                );
                run_esc(exic, s.as_bytes())
            }
            "let" => {
                let s = format!(
                    "{}\n{}\n{}\n{}\n{}",
                    v["binding_name"].as_str().unwrap(),
                    v["binding_type"].as_str().unwrap(),
                    v["body"].as_str().unwrap(),
                    v["value"].as_str().unwrap(),
                    v["type"].as_str().unwrap()
                );
                run_esc(exlet, s.as_bytes())
            }
            "call" => {
                let args_body = v["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| format!("\"{}\"", a.as_str().unwrap()))
                    .collect::<Vec<_>>()
                    .join(",");
                let s = format!(
                    "{}\n{}\n{}",
                    args_body,
                    v["symbol"].as_str().unwrap(),
                    v["type"].as_str().unwrap()
                );
                run_esc(excall, s.as_bytes())
            }
            _ => continue, // any other Expression kind
        };
        assert_eq!(
            got.as_slice(),
            hash.as_bytes(),
            "Expression mismatch for {payload}"
        );
        checked += 1;
    }
    assert!(
        checked >= 8,
        "expected several Expressions across kinds; checked {checked}"
    );
}

/// Import + verify + build the self-hosted importer (lib.cdb + import.cdb) once per
/// test process; the shared `(TempDir, db, exe)` is reused by `importer()` (the native
/// binary) and `importer_db()` (the database, for inspecting the importer's compiled form).
fn importer_artifacts() -> &'static (TempDir, PathBuf, PathBuf) {
    static IMPORTER: OnceLock<(TempDir, PathBuf, PathBuf)> = OnceLock::new();
    IMPORTER.get_or_init(|| {
        let temp = tempdir().unwrap();
        let db = temp.path().join("selfhost-import.sqlite");
        run(&["init", path(&db)]);
        run(&["import", path(&db), "compiler/front/lib.cdb"]);
        run(&["import", path(&db), "compiler/front/import.cdb"]);
        run(&["verify", path(&db)]);
        let exe = temp.path().join("import-bin");
        run(&["build", path(&db), "main", "--out", path(&exe)]);
        (temp, db, exe)
    })
}

/// The native importer binary (minimal-grammar source -> root hash).
fn importer() -> &'static Path {
    importer_artifacts().2.as_path()
}

/// The database the importer was built from (lib.cdb + import.cdb imported), for
/// inspecting the importer's own compiled form — e.g. its per-function stack frames.
fn importer_db() -> &'static Path {
    importer_artifacts().1.as_path()
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
fn importer_reproduces_the_root_hash_for_three_function_programs() {
    // 15a.4: the n-function create_function chain at n = 3. The importer parses THREE top-level
    // functions (the previous path parsed exactly two and mis-parsed a third), orders them
    // canonically (alphabetical for independent functions), runs the TWO-migration history
    // chain (each non-first symbol born at the previous migration's running history — the
    // second migration is the first to carry a non-empty parent history), and assembles the
    // three-symbol ProgramRoot with the general-m assembler (names display-ordered,
    // symbols/param_names symbol-hash-ordered, the two orders diverging in general). Scope:
    // three INDEPENDENT functions (the DAG toposort is the next increment); covers parameters,
    // a bool return, source order != canonical order, and assorted/larger literal values.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        "fn a() -> i64 = 1\nfn b() -> i64 = 2\nfn c() -> i64 = 3\n",
        // source order != canonical (the importer must sort to a(0), b(1), c(2))
        "fn c() -> i64 = 3\nfn a() -> i64 = 1\nfn b() -> i64 = 2\n",
        "fn b() -> i64 = 2\nfn c() -> i64 = 3\nfn a() -> i64 = 1\n",
        // a parameter-bearing function, a bool return, mixed
        "fn a(x: i64) -> i64 = x + 1\nfn b() -> bool = true\nfn c() -> i64 = 99\n",
        "fn p(a: i64, b: i64) -> i64 = a + b\nfn q() -> i64 = 7\nfn r() -> i64 = 8\n",
        // reverse-alphabetical source order
        "fn z() -> i64 = 100\nfn y() -> i64 = 200\nfn x() -> i64 = 300\n",
        // longer names + a larger literal value
        "fn alpha() -> i64 = 1000000\nfn beta() -> i64 = 2\nfn gamma() -> i64 = 3\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-three-{i}.sqlite"));
        let src = temp.path().join(format!("ref-three-{i}.cdb"));
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
            "self-hosted importer three-function root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_three_function_dags() {
    // 15a.4 (Inc 1.2): the n-function chain at n = 3 with DEPENDENCY EDGES. A call adds an edge,
    // so the canonical (creation) order is a Kahn toposort — a callee is created BEFORE its caller,
    // with an alphabetical tie-break — which in general DIVERGES from the alphabetical display
    // order. The importer must build the call graph (body_calls over each function's body span),
    // toposort it, run the two-migration chain in toposort order (the second migration is the first
    // to RAW-serialize a call body, exercising raw_call), and assemble the three-symbol root with
    // names display-ordered but symbols/param_names symbol-hash-ordered. Covers a linear chain
    // (both source orders), fan-out, fan-in, a mixed mid+tail caller, call arguments using a
    // callee, and a bool-returning callee.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // linear chain: c is created first, then b (calls c), then a (calls b)
        "fn c() -> i64 = 1\nfn b() -> i64 = c()\nfn a() -> i64 = b()\n",
        // same chain written in reverse source order (a, b, c) -> same canonical order (c, b, a)
        "fn a() -> i64 = b()\nfn b() -> i64 = c()\nfn c() -> i64 = 1\n",
        // fan-out: both a and b call c -> c first, then a, b (alphabetical among the ready)
        "fn c() -> i64 = 7\nfn a() -> i64 = c()\nfn b() -> i64 = c()\n",
        // fan-in: main calls both a and b -> a, b first (alphabetical), then main
        "fn a() -> i64 = 1\nfn b() -> i64 = 2\nfn main() -> i64 = a() + b()\n",
        // mid + tail call: x first, then y (calls x), then z (calls y and x)
        "fn x() -> i64 = 5\nfn y() -> i64 = x()\nfn z() -> i64 = y() + x()\n",
        // a call with arguments + a callee with a parameter (the raw_call body carries args)
        "fn inc(n: i64) -> i64 = n + 1\nfn base() -> i64 = 10\nfn top() -> i64 = inc(base())\n",
        // a bool-returning callee used by two callers
        "fn flag() -> bool = true\nfn use1() -> bool = flag()\nfn use2() -> bool = flag()\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-dag3-{i}.sqlite"));
        let src = temp.path().join(format!("ref-dag3-{i}.cdb"));
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
            "self-hosted importer three-function DAG root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_four_plus_function_programs() {
    // 15a.4 (Inc 1.3): the n-function create_function chain GENERALIZED beyond three. The chain is
    // a loop-fold (bt0 + chain_mid recursion over the middle functions + bt_last) over the
    // toposort-canonical order, building n-1 migrations; the extra-callee table is multi-entry (a
    // diamond's last function calls TWO earlier non-first functions, each born at its own running
    // history). The general toposort (adjN/kahnN, n up to 8) and the general assembler drive it.
    // Covers independent programs (4, 5, 6 functions; forward and reverse source order) and DAGs
    // (a 4-chain, a diamond, a 3-way fan-in, and a 5-function mixed chain with call arguments).
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // independent
        "fn a() -> i64 = 1\nfn b() -> i64 = 2\nfn c() -> i64 = 3\nfn d() -> i64 = 4\n",
        "fn e() -> i64 = 5\nfn d() -> i64 = 4\nfn c() -> i64 = 3\nfn b() -> i64 = 2\nfn a() -> i64 = 1\n",
        "fn a() -> i64 = 1\nfn b() -> i64 = 2\nfn c() -> i64 = 3\nfn d() -> i64 = 4\nfn e() -> i64 = 5\nfn f() -> i64 = 6\n",
        // DAG: a 4-function linear chain (canonical order d, c, b, a)
        "fn d() -> i64 = 1\nfn c() -> i64 = d()\nfn b() -> i64 = c()\nfn a() -> i64 = b()\n",
        // DAG: a diamond — `a` calls BOTH `b` and `c` (two non-first callees -> multi-entry table)
        "fn d() -> i64 = 1\nfn b() -> i64 = d()\nfn c() -> i64 = d()\nfn a() -> i64 = b() + c()\n",
        // DAG: a 3-way fan-in (main calls a, b, c)
        "fn a() -> i64 = 1\nfn b() -> i64 = 2\nfn c() -> i64 = 3\nfn main() -> i64 = a() + b() + c()\n",
        // DAG: a 5-function mixed chain with call arguments
        "fn inc(n: i64) -> i64 = n + 1\nfn base() -> i64 = 10\nfn mid() -> i64 = inc(base())\nfn hi() -> i64 = inc(mid())\nfn top() -> i64 = hi() + mid()\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-nfn-{i}.sqlite"));
        let src = temp.path().join(format!("ref-nfn-{i}.cdb"));
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
            "self-hosted importer n-function root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_three_member_cliques() {
    // 15a.4 (Inc 2): a THREE-member DISCRETIZING recursion clique — three mutually-recursive
    // functions forming a SINGLE 3-SCC whose 1-WL colour refinement gives three distinct colours.
    // The importer detects the single 3-SCC (single3scc over the call-adjacency mask, distinguishing
    // it from a DAG, a 2-SCC + singleton, or a self-loop + independents), runs the Weisfeiler-Leman
    // refinement (clique3_order: each round colours a member by hash("codedb/recursion-order/v1\0" |
    // static_sig | its raw body with peer calls recoloured to "@recursion-peer:<peer colour>", until
    // the distinct-colour count stabilizes), sorts the members by their converged colour into their
    // recursion_group:<ordinal> nonces, and emits ONE create_recursion_group with the three members
    // in WL-ordinal order (names display-ordered, param_names/symbols symbol-hash-ordered). Both
    // births are genesis, so the root is reproduced from objects alone (no migration chain). A
    // member's call to a peer resolves to that peer's ordinal via the packed alpha-rank -> ordinal
    // map (parse_call's base-4 `aord` decode, generalizing the two-member shortcut). Non-discretizing
    // (symmetric) cliques and mixed SCCs are out of scope (clique_label_search, a later increment).
    // Covers asymmetric cliques whose WL order differs from BOTH alphabetical and source order,
    // parameter-bearing members called with arguments, bool members, if / arithmetic / comparison
    // bodies, and source-order permutations (the canonical order is order-independent).
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // parameters + if + cross-member calls with arguments; WL order [c, a, b]
        "fn a(n: i64) -> i64 = if n == 0 then 1 else b(n - 1)\nfn b(n: i64) -> i64 = if n == 0 then 2 else c(n - 1)\nfn c(n: i64) -> i64 = if n == 0 then 3 else a(n - 1)\n",
        // arithmetic bodies; WL order [a, b, c]
        "fn a() -> i64 = b() + 1\nfn b() -> i64 = c() * 2\nfn c() -> i64 = a()\n",
        // WL order [y, z, x] (none of source, alphabetical)
        "fn x() -> i64 = y() + 10\nfn y() -> i64 = z() + 20\nfn z() -> i64 = x()\n",
        // an if with a comparison whose operand is a peer call; WL order [c, a, b]
        "fn a() -> i64 = if b() == 0 then 0 else 1\nfn b() -> i64 = c()\nfn c() -> i64 = a()\n",
        // repeated peer calls (a calls b twice, c calls a twice); WL order [c, b, a]
        "fn a() -> i64 = b() + b()\nfn b() -> i64 = c()\nfn c() -> i64 = a() + a()\n",
        // a member whose body compares two distinct peers; WL order [p, q, r]
        "fn p() -> i64 = q()\nfn q() -> i64 = r() + 1\nfn r() -> i64 = if p() == q() then 0 else 1\n",
        // SOURCE-ORDER PERMUTATIONS of one clique — canonical (WL) order is order-independent
        "fn c() -> i64 = a()\nfn a() -> i64 = b() + 1\nfn b() -> i64 = c() * 2\n",
        "fn b() -> i64 = c() * 2\nfn c() -> i64 = a()\nfn a() -> i64 = b() + 1\n",
        // bool members, longer names, an if with a bool-literal condition
        "fn first() -> bool = second()\nfn second() -> bool = if true then third() else first()\nfn third() -> bool = first()\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-clq3-{i}.sqlite"));
        let src = temp.path().join(format!("ref-clq3-{i}.cdb"));
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
            "self-hosted importer three-member-clique root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_n_member_cliques() {
    // 15a.4 (Inc 3): a DISCRETIZING recursion clique of FOUR OR MORE members (n in [4, 8]). The WL
    // refinement generalizes from the n=3 unrolled form to a RECURSION over n members with the n
    // colours held as a concat of 71-char hashes (each round recoloured by reading a peer's colour
    // slice). A single n-member SCC is detected by `single_scc_n` (node 0 reaches everyone and
    // everyone reaches node 0). The genuinely new wrinkle versus n = 3 is the REAL symbol-hash
    // member ordering: the recursion_group:<ordinal> symbol hashes are no longer in ordinal order
    // for n >= 4 (they coincide only at n = 3), so the RecursionGroup object's members AND the
    // ProgramRoot's param_names / symbols arrays are sorted by SYMBOL HASH (via hash_perm) while
    // names stays display-ordered. A peer call resolves to its ordinal through the base-8 packed
    // alpha-rank -> ordinal map (3 bits per ordinal, up to eight members). Both births are genesis,
    // so the root is built from objects alone (no migration chain). Covers n = 4/5/6/8, source-order
    // permutations, parameters called with arguments, bool members, and if / arithmetic bodies.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // n = 4 four-cycle (arithmetic bodies)
        "fn a() -> i64 = b() + 1\nfn b() -> i64 = c() * 2\nfn c() -> i64 = d()\nfn d() -> i64 = a()\n",
        // n = 4 with parameters, if, and cross-member calls with arguments
        "fn a(n: i64) -> i64 = if n == 0 then 1 else b(n - 1)\nfn b(n: i64) -> i64 = if n == 0 then 2 else c(n - 1)\nfn c(n: i64) -> i64 = if n == 0 then 3 else d(n - 1)\nfn d(n: i64) -> i64 = if n == 0 then 4 else a(n - 1)\n",
        // n = 4 source-order permutation of the four-cycle (canonical order is order-independent)
        "fn c() -> i64 = d()\nfn d() -> i64 = a()\nfn a() -> i64 = b() + 1\nfn b() -> i64 = c() * 2\n",
        // n = 5
        "fn a() -> i64 = b() + 1\nfn b() -> i64 = c() + 2\nfn c() -> i64 = d() + 3\nfn d() -> i64 = e() + 4\nfn e() -> i64 = a()\n",
        // n = 6
        "fn a() -> i64 = b() + 1\nfn b() -> i64 = c() + 2\nfn c() -> i64 = d() + 3\nfn d() -> i64 = e() + 4\nfn e() -> i64 = f() + 5\nfn f() -> i64 = a()\n",
        // n = 8 (the packed-spans / base-8-aord ceiling)
        "fn a() -> i64 = b() + 1\nfn b() -> i64 = c() + 2\nfn c() -> i64 = d() + 3\nfn d() -> i64 = e() + 4\nfn e() -> i64 = f() + 5\nfn f() -> i64 = g() + 6\nfn g() -> i64 = h() + 7\nfn h() -> i64 = a()\n",
        // n = 4 bool members, longer names, an if with a bool-literal condition
        "fn first() -> bool = if true then second() else first()\nfn second() -> bool = third()\nfn third() -> bool = fourth()\nfn fourth() -> bool = first()\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-clqn-{i}.sqlite"));
        let src = temp.path().join(format!("ref-clqn-{i}.cdb"));
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
            "self-hosted importer n-member-clique root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_symmetric_cliques() {
    // 15a.4 (Inc 4): a NON-DISCRETIZING (vertex-symmetric) recursion clique — one whose 1-WL colour
    // refinement does NOT separate the members (every member is structurally identical: a pure directed
    // n-cycle, or the complete clique K_n). The importer falls back from the colour sort to
    // individualization-refinement (clique_label_search): refine to stability (preserve_own), and at a
    // discrete leaf score the labeling by its canonical FORM (peer calls -> ordinals) with the member-
    // name sequence as the automorphism-orbit tie-break; the lex-min (form, key) over every branch is
    // the order. The keystone is that the 1-WL SEED must byte-match the Rust importer:
    // refine_clique_colors stops the moment a round fails to RAISE the distinct count, so a symmetric
    // clique's seed is round 1 (all-one-colour), NOT a re-hashed later round — and the whole search is
    // seeded from it, so an off-by-one round there changes the colour VALUES and the chosen rotation.
    // Covers pure cycles n = 3/4/5/6/8, the fully-symmetric K3, bool members, and source-order
    // permutations (the canonical order is order-independent).
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // n = 3 pure cycle (i64) — 1-WL stays one colour; individualization picks the reverse rotation
        "fn a() -> i64 = b()\nfn b() -> i64 = c()\nfn c() -> i64 = a()\n",
        // n = 3 pure cycle, source-order permuted (canonical order is order-independent)
        "fn c() -> i64 = a()\nfn a() -> i64 = b()\nfn b() -> i64 = c()\n",
        // n = 3 pure cycle, bool members (same structure, different colours -> a different rotation)
        "fn a() -> bool = b()\nfn b() -> bool = c()\nfn c() -> bool = a()\n",
        // the fully-symmetric K3: each member calls BOTH others (automorphism group S3, so the search
        // individualizes more than once before the partition discretizes)
        "fn a() -> i64 = b() + c()\nfn b() -> i64 = c() + a()\nfn c() -> i64 = a() + b()\n",
        // n = 4 pure cycle
        "fn a() -> i64 = b()\nfn b() -> i64 = c()\nfn c() -> i64 = d()\nfn d() -> i64 = a()\n",
        // n = 5 pure cycle
        "fn a() -> i64 = b()\nfn b() -> i64 = c()\nfn c() -> i64 = d()\nfn d() -> i64 = e()\nfn e() -> i64 = a()\n",
        // n = 6 pure cycle
        "fn a() -> i64 = b()\nfn b() -> i64 = c()\nfn c() -> i64 = d()\nfn d() -> i64 = e()\nfn e() -> i64 = f()\nfn f() -> i64 = a()\n",
        // n = 8 pure cycle (the packed-spans / base-8-aord ceiling)
        "fn a() -> i64 = b()\nfn b() -> i64 = c()\nfn c() -> i64 = d()\nfn d() -> i64 = e()\nfn e() -> i64 = f()\nfn f() -> i64 = g()\nfn g() -> i64 = h()\nfn h() -> i64 = a()\n",
        // n = 4 pure cycle, bool members
        "fn a() -> bool = b()\nfn b() -> bool = c()\nfn c() -> bool = d()\nfn d() -> bool = a()\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-symclq-{i}.sqlite"));
        let src = temp.path().join(format!("ref-symclq-{i}.cdb"));
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
            "self-hosted importer symmetric-clique root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_single_record() {
    // 15a (object-kind breadth): a single `record` type definition — the first non-function
    // object kind. A type-only program (no functions) has empty symbols/names/param_names and
    // populates the root's `types` + `type_names` arrays. The type symbol's birth is genesis
    // (local_nonce "import:type:main:<Name>:0") and each field is a `record_field` SymbolBirth
    // OWNED by the type (nonce "<seed>:field:<idx>:<name>"); the RecordDef carries the fields in
    // DECLARATION order with their scalar field-type hashes, and the TypeDef wraps it. Every
    // birth is genesis, so the root is built from objects ALONE — no migration/history chain,
    // exactly like a single function or a single-member recursion group. Root-hash equality is
    // an exact gate: any field order, birth seed, key order, or canonical-payload error changes
    // a child object hash and so the root. Covers 1..8 fields and the i64/bool/sized scalars.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        "record Point {\n  x: i64\n  y: i64\n}\n",
        "record W {\n  v: i64\n}\n",
        "record Three {\n  a: i64\n  b: i64\n  c: i64\n}\n",
        "record Mixed {\n  flag: bool\n  n: i64\n}\n",
        "record Sized {\n  b: u8\n  w: u32\n  big: u64\n}\n",
        "record Bools {\n  p: bool\n  q: bool\n}\n",
        "record LongName {\n  the_first_field: i64\n  another: bool\n}\n",
        "record R {\n  only_bool: bool\n}\n",
        "record AllInts {\n  a: i8\n  b: i16\n  c: i32\n  d: i64\n  e: u8\n  f: u16\n  g: u32\n  h: u64\n}\n",
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
            "self-hosted importer single-record root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_single_enum() {
    // 15a (object-kind breadth): a single `enum` type definition — structurally identical to a
    // record (the same `name: type` member grammar; this grammar has no payload-free variants),
    // differing only in the object framing: EnumDef vs RecordDef, `variants`/`variant_symbol`
    // vs `fields`/`field_symbol`, the `enum_variant` member-symbol kind, the `:variant:` birth-
    // seed tag, and the TypeDef `type_kind` "enum". Same genesis-birth / no-chain root from
    // objects alone. One kind-parameterized builder produces both; this gate pins the enum
    // strings (a wrong tag/key/kind would reproduce records but not enums).
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        "enum Sign {\n  neg: i64\n  pos: bool\n}\n",
        "enum IoResult {\n  ok: i64\n  err: i64\n}\n",
        "enum One {\n  only: i64\n}\n",
        "enum Many {\n  a: u8\n  b: u16\n  c: u32\n  d: u64\n}\n",
        "enum Choice {\n  left: bool\n  right: bool\n  middle: i64\n}\n",
        "enum E {\n  just_one_variant: u64\n}\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-enum-{i}.sqlite"));
        let src = temp.path().join(format!("ref-enum-{i}.cdb"));
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
            "self-hosted importer single-enum root mismatch for `{}`",
            source.trim()
        );
    }
}

#[test]
fn importer_reproduces_the_root_hash_for_n_type_definitions() {
    // 15a (object-kind breadth): N >= 2 independent record/enum type definitions, the create_type
    // migration/history CHAIN — the analog of the n-function create_function chain. The types are
    // ordered canonically (alphabetical, since independent scalar-field types have no dependency
    // edges), and each NON-first type's type symbol AND its field/variant symbols are born at the
    // RUNNING history (genesis for the alpha-first), seeded by a chain of create_type migrations.
    // The multi-type ProgramRoot's `type_names` is display(name)-ordered but `types` is
    // type_symbol-HASH-ordered — they diverge, the same names-vs-hash split as the function root —
    // so the dual ordering is genuinely exercised. Root-hash equality is an exact gate over the
    // whole chain: any migration_hash/history_hash/birth-seed/canonical-order/key-order error
    // changes a downstream birth -> symbol -> the root. Covers 2..5 types, records + enums + mixed,
    // both source orders (the root is order-independent), and sized field types.
    if !can_build_default_native_target() {
        return;
    }
    let exe = importer();
    let temp = tempdir().unwrap();
    let sources = [
        // two records (independent) -> two create_type migrations
        "record Alpha {\n  x: i64\n}\nrecord Beta {\n  y: bool\n}\n",
        // reversed source order -> the SAME root (canonical ordering is order-independent)
        "record Beta {\n  y: bool\n}\nrecord Alpha {\n  x: i64\n}\n",
        // record + enum mixed
        "record A {\n  a: i64\n  b: i64\n}\nenum B {\n  ok: i64\n  err: bool\n}\n",
        // three types, alphabetical order != source order
        "record Zeta {\n  z: u8\n}\nrecord Mid {\n  m: i64\n}\nrecord Apex {\n  a: bool\n}\n",
        // three mixed (the type_names-display vs types-hash divergence)
        "enum Color {\n  r: u8\n  g: u8\n  b: u8\n}\nrecord Point {\n  x: i64\n  y: i64\n}\nenum Flag {\n  on: bool\n}\n",
        // four records in reverse-alphabetical source order
        "record D {\n  d: i64\n}\nrecord C {\n  c: i64\n}\nrecord B {\n  b: i64\n}\nrecord A {\n  a: i64\n}\n",
        // five records (a longer chain)
        "record R0 {\n  f: i64\n}\nrecord R1 {\n  f: i64\n}\nrecord R2 {\n  f: i64\n}\nrecord R3 {\n  f: i64\n}\nrecord R4 {\n  f: i64\n}\n",
    ];
    for (i, source) in sources.iter().enumerate() {
        let db = temp.path().join(format!("ref-ntype-{i}.sqlite"));
        let src = temp.path().join(format!("ref-ntype-{i}.cdb"));
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
            "self-hosted importer n-type-chain root mismatch for:\n{source}"
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

#[test]
fn frame_report_keeps_importer_functions_within_the_arm64_frame_limit() {
    // frame-report (Phase 15a.5.2) is the offline, per-function mirror of the v0 arm64
    // frame-size bail: every function in the self-hosted importer's own closure must fit
    // the 4095-byte v0 arm64 stack frame, or `build` would reject it. Reuses the importer
    // database so no second import of import.cdb is paid.
    if !can_build_default_native_target() {
        return;
    }
    let out = run(&["frame-report", path(importer_db()), "main", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&out).expect("frame-report json");

    assert_eq!(report["target"], "aarch64-apple-darwin");
    assert_eq!(report["frame_limit"], 4095);
    assert_eq!(report["param_cap"], 8);

    let functions = report["functions"].as_array().expect("functions array");
    assert!(
        functions.len() > 50,
        "expected the importer's many functions, got {}",
        functions.len()
    );

    // The gate: nothing over the frame limit or the machine-parameter cap. On failure the
    // JSON names the offending functions and their sizes.
    assert_eq!(
        report["over_frame_limit_count"], 0,
        "an importer function exceeds the v0 arm64 frame limit:\n{out}"
    );
    assert_eq!(
        report["over_param_cap_count"], 0,
        "an importer function exceeds the v0 arm64 machine-parameter cap:\n{out}"
    );

    let max = report["max_stack_size"].as_u64().expect("max_stack_size");
    assert!(
        (1..=4095).contains(&max),
        "max importer frame {max} is outside the expected (0, 4095] range"
    );

    // Functions are listed largest-frame first.
    let sizes: Vec<u64> = functions
        .iter()
        .map(|f| f["stack_size"].as_u64().expect("stack_size"))
        .collect();
    assert!(
        sizes.windows(2).all(|w| w[0] >= w[1]),
        "frame-report functions should be sorted by descending frame size"
    );
}

#[test]
fn frame_report_flags_an_over_limit_frame_before_the_native_build_rejects_it() {
    // A function with a large aggregate local overflows the v0 arm64 stack frame.
    // frame-report flags it offline with an actionable diagnostic, and the native build
    // genuinely rejects the same frame — so the report mirrors a real build-time bail, not
    // a guessed threshold.
    let temp = tempdir().unwrap();
    let db = temp.path().join("over.sqlite");
    let src = temp.path().join("over.cdb");
    std::fs::write(
        &src,
        "fn main() -> i64 = let a: array<i64, 600> = [0; 600] in a[0]\n",
    )
    .unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let out = run(&["frame-report", path(&db), "main", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&out).expect("frame-report json");
    assert_eq!(
        report["over_frame_limit_count"], 1,
        "frame-report should flag the oversized frame:\n{out}"
    );
    let main_fn = report["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .find(|f| f["name"] == "main.main")
        .expect("main.main in the report");
    assert_eq!(main_fn["over_frame_limit"], true);
    assert!(
        main_fn["stack_size"].as_u64().expect("stack_size") > 4095,
        "the oversized frame should report a stack_size over the limit"
    );

    // The native arm64 build rejects the very same frame today, with the diagnostic the
    // report mirrors.
    if can_build_default_native_target() {
        let exe = temp.path().join("over-bin");
        let output = bin()
            .args(["build", path(&db), "main", "--out", path(&exe)])
            .assert()
            .failure()
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("stack frame is too large"),
            "build should fail with the frame diagnostic, got:\n{stderr}"
        );
    }
}
