// Phase 16 (R12) acceptance: process arguments. The `arg_count`/`arg_len`/
// `arg_byte` builtins read the process's command-line arguments (program name
// excluded) — `io`-effected ambient input. Natively the cc link harness
// captures argc/argv and the lowered ops call its runtime accessors
// (`codedb_arg_*`, the malloc/free platform-symbol pattern); under the
// reference evaluator the CLI seeds the same list via `--process-arg`, so the
// acceptance oracle is eval == native ON THE SAME ARGUMENTS. `std.io.
// arg_string` composes the byte reads into an owned string with a move-only
// loop accumulator (loop-carried drop glue).

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

fn run_fail(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
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

const ECHO_SOURCE: &str = "\
fn arg_total() -> i64 effects[io] = arg_count()\n\
fn first_byte() -> i64 effects[io] = to_i64(arg_byte(0, 0))\n\
fn summary() -> i64 effects[io] =\n\
  arg_count() * 100 + arg_len(0) * 10 + to_i64(arg_byte(0, 0)) - 100\n\
fn arg_string(i: i64) -> string effects[io, alloc, state] =\n\
  loop buf = string_with_capacity(arg_len(i)) while string_len(buf) < arg_len(i) do\n\
    let p: unit = string_push(buf, arg_byte(i, string_len(buf))) in buf\n\
fn second_arg_len() -> i64 effects[io, alloc, state] =\n\
  let s: string = arg_string(1) in string_len(s)\n\
fn main() -> i64 effects[io] = summary()\n";

#[test]
fn argv_builtins_agree_between_eval_and_native() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("argv.sqlite");
    let src = temp.path().join("argv.cdb");
    std::fs::write(&src, ECHO_SOURCE).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    run(&["verify", path(&db)]);

    // Reference evaluator with seeded args: ["zap", "qq"].
    // summary = 2*100 + 3*10 + 'z'(122) - 100 = 252.
    let eval = |entry: &str| {
        run(&[
            "eval",
            path(&db),
            entry,
            "--process-arg",
            "zap",
            "--process-arg",
            "qq",
        ])
        .trim()
        .to_string()
    };
    assert_eq!(eval("summary"), "252");
    assert_eq!(eval("first_byte"), "122");
    assert_eq!(eval("second_arg_len"), "2");
    // Un-seeded evaluation deterministically sees zero arguments.
    assert_eq!(run(&["eval", path(&db), "arg_total"]).trim(), "0");
    // Out of range is a clean error, the parity of the native trap.
    let err = run_fail(&["eval", path(&db), "first_byte"]);
    assert!(
        err.contains("out of range"),
        "expected a range error, got: {err}"
    );

    // Trace agrees with eval on the same seeded args.
    let trace = parse_json(&run(&[
        "trace",
        path(&db),
        "first_byte",
        "--process-arg",
        "zap",
        "--json",
    ]));
    assert_eq!(trace["status"], "ok", "{trace}");
    assert_eq!(trace["result"]["value"], "122");

    if !can_build_default_native_target() {
        return;
    }
    // The acceptance echo: the NATIVE binary started with the same arguments
    // returns the same value the evaluator produced.
    let exe = temp.path().join("argv_bin");
    run(&["build", path(&db), "main", "--out", path(&exe)]);
    let output = StdCommand::new(&exe)
        .args(["zap", "qq"])
        .output()
        .expect("run native argv binary");
    assert_eq!(output.status.code(), Some(252), "native echo exit code");
    // No arguments: the runtime traps on the out-of-range read (fail-stop),
    // never returning a fabricated value.
    let bare = StdCommand::new(&exe).output().expect("run bare binary");
    assert!(
        !bare.status.success(),
        "bare run must trap, got {:?}",
        bare.status
    );
}

#[test]
fn argv_requires_the_io_effect_and_round_trips() {
    // The builtins are ambient input, so a pure signature is rejected.
    let temp = tempdir().unwrap();
    let db = temp.path().join("pure.sqlite");
    let src = temp.path().join("pure.cdb");
    std::fs::write(&src, "fn sneaky() -> i64 = arg_count()\n").unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        err.contains("requires undeclared effect io"),
        "expected an io-effect rejection, got: {err}"
    );

    // Projection fixpoint: the builtins render as calls and re-import to the
    // same root (SPEC_V3 §11).
    let db2 = temp.path().join("rt.sqlite");
    let src2 = temp.path().join("rt.cdb");
    std::fs::write(&src2, ECHO_SOURCE).unwrap();
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&src2)]);
    let export1 = temp.path().join("rt.export1.cdb");
    run(&["export", path(&db2), "--branch", "main", "--out", path(&export1)]);
    let exported = std::fs::read_to_string(&export1).unwrap();
    assert!(exported.contains("arg_byte(0, 0)"), "{exported}");

    let db3 = temp.path().join("rt2.sqlite");
    run(&["init", path(&db3)]);
    run(&["import", path(&db3), path(&export1)]);
    run(&["verify", path(&db3)]);
    let root = |db: &Path| {
        parse_json(&run(&["history", path(db), "--json"]))["root_hash"]
            .as_str()
            .expect("root_hash")
            .to_string()
    };
    assert_eq!(root(&db2), root(&db3), "import→export→import fixpoint");
}

#[test]
fn argv_capability_is_surfaced_in_the_build_plan() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("caps.sqlite");
    let src = temp.path().join("caps.cdb");
    std::fs::write(&src, ECHO_SOURCE).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    let plan = parse_json(&run(&["build-plan", path(&db), "main", "--json"]));
    let capabilities = plan["capabilities"].as_array().expect("capabilities");
    assert!(
        capabilities
            .iter()
            .any(|capability| capability["name"] == "args"),
        "expected the args capability in the build plan: {plan}"
    );
    let platform = plan["platform_external_symbols"]
        .as_array()
        .expect("platform externals");
    for link_name in ["codedb_arg_count", "codedb_arg_len", "codedb_arg_byte"] {
        assert!(
            platform
                .iter()
                .any(|symbol| symbol["link_name"] == link_name),
            "expected {link_name} in platform externals: {plan}"
        );
    }
}
