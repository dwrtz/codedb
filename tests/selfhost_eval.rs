// Phase 8 (ladder rung 0) Stage 1: the CodeDB-hosted evaluator's loader.
//
// `compiler/eval/eval.cdb` is imported, verified, and built to a NATIVE
// binary, then fed real `emit-cir` artifacts on stdin. Its Stage-1 probe
// prints five numbers - function count, entry index, entry op count, entry
// param count, entry local count - which must equal the emission summary's
// values: the probe only gets them right if the .cdb loader walked the
// header, both pools, the function table, and the entry's section tables
// (layout/type/value/param/local rows, all fixed-width) at the exact byte
// offsets the Rust encoder produced. Non-CIR stdin exits 65, fail-closed.
//
// The full three-way result oracle (CodeDB-eval == Rust-eval == native) is
// the later stages' gate; this pins the substrate they decode with.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};

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

/// Import the committed evaluator sources, verify, and build the native
/// evaluator binary.
fn build_evaluator(temp: &Path) -> PathBuf {
    let db = temp.join("selfhost-eval.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "std/fmt.cdb"]);
    run(&["import", path(&db), "compiler/eval/eval.cdb"]);
    run(&["verify", path(&db)]);
    let exe = temp.join("eval-bin");
    run(&["build", path(&db), "main", "--out", path(&exe)]);
    exe
}

/// Emit the CIR artifact for `entry` of a committed example and return the
/// artifact path plus the emission summary the probe is checked against.
fn emit_example_cir(temp: &Path, name: &str, source: &str, entry: &str) -> (PathBuf, JsonValue) {
    let db = temp.join(format!("{name}.sqlite"));
    run(&["init", path(&db)]);
    run(&["import", path(&db), source]);
    let cir = temp.join(format!("{name}.cir"));
    let summary = parse_json(&run(&[
        "emit-cir",
        path(&db),
        entry,
        "--out",
        path(&cir),
        "--json",
    ]));
    (cir, summary)
}

/// Run the native evaluator with `input` on stdin; return (exit code, stdout).
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

fn probe_lines(stdout: &str) -> Vec<i64> {
    stdout
        .lines()
        .map(|line| {
            line.parse::<i64>()
                .unwrap_or_else(|err| panic!("non-numeric probe line {line:?}: {err}"))
        })
        .collect()
}

#[test]
fn stage1_probe_matches_the_emission_summary_for_real_examples() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let exe = build_evaluator(temp.path());

    // Three entries with different table shapes: tokenize_ok (closure of 4
    // fns), scan (4 params - exercises the param-row walk), and sha256's
    // digest_0 (12 fns, real layout/type tables, a later function-table row).
    for (name, source, entry) in [
        ("tokenizer", "examples/v3/tokenizer.cdb", "tokenize_ok"),
        ("tokenizer-scan", "examples/v3/tokenizer.cdb", "scan"),
        ("sha256", "examples/v3/sha256.cdb", "digest_0"),
    ] {
        let (cir, summary) = emit_example_cir(temp.path(), name, source, entry);
        let (code, stdout) = run_evaluator(&exe, &cir);
        assert_eq!(code, 0, "{name}: probe exit code, stdout:\n{stdout}");
        let expected: Vec<i64> = [
            "function_count",
            "entry_index",
            "entry_op_count",
            "entry_param_count",
            "entry_local_count",
        ]
        .iter()
        .map(|key| {
            summary[key]
                .as_i64()
                .unwrap_or_else(|| panic!("{name}: summary key {key} missing: {summary}"))
        })
        .collect();
        assert_eq!(probe_lines(&stdout), expected, "{name}: probe vs summary");
    }
}

#[test]
fn stage1_probe_rejects_non_cir_input_fail_closed() {
    if !can_build_default_native_target() {
        return;
    }
    let temp = tempdir().unwrap();
    let exe = build_evaluator(temp.path());

    // Empty input: too short to even carry the magic.
    let empty = temp.path().join("empty.bin");
    std::fs::write(&empty, b"").unwrap();
    let (code, stdout) = run_evaluator(&exe, &empty);
    assert_eq!((code, stdout.as_str()), (65, ""), "empty input");

    // Long enough, but the magic is wrong.
    let garbage = temp.path().join("garbage.bin");
    std::fs::write(&garbage, b"this is definitely not a CIR artifact").unwrap();
    let (code, stdout) = run_evaluator(&exe, &garbage);
    assert_eq!((code, stdout.as_str()), (65, ""), "non-CIR input");
}
