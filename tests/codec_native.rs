// Phase 9 acceptance (PLAN_V3): the codec forcing-function programs that drove
// the sized-integer / bitwise / cast stack (R5/R4/R6/R2) compile to native
// artifacts and match their reference outputs. `fnv1a.cdb` no longer emulates
// xor/wrap with i64 arithmetic — it is a real `u32` fold with bitwise XOR, a
// wrapping 32-bit multiply, and a `to_u32` cast — and still yields 0x1e225c96.

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

#[test]
fn fnv1a_hashes_codedb_to_reference_digest_native() {
    // fnv1a32("codedb") = 505568406 = 0x1e225c96
    const EXPECT: &str = "505568406";
    let temp = tempdir().unwrap();
    let db = temp.path().join("fnv1a.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/fnv1a.cdb"]);
    assert_eq!(
        run(&["eval", path(&db), "main"]).trim(),
        EXPECT,
        "fnv1a evaluator digest"
    );
    run(&["verify", path(&db)]);

    if !can_build_default_native_target() {
        return;
    }
    let created = parse_json(&run(&[
        "create-test",
        path(&db),
        "fnv1a_native",
        "--entry",
        "main",
        "--expect-int",
        &format!("u32:{EXPECT}"),
        "--native-required",
        "--json",
    ]));
    assert_eq!(created["status"], "applied");
    let report = parse_json(&run(&["test", path(&db), "--json"]));
    assert_eq!(report["status"], "passed", "fnv1a native run: {report}");
    assert_eq!(report["native_mismatches"], 0, "fnv1a native mismatch");
}

#[test]
fn sha256_hashes_abc_to_reference_digest() {
    // sha256("abc") = ba7816bf 8f01cfea 414140de 5dae2223 b00361a3 96177a9c
    //                 b410ff61 f20015ad — each 32-bit word in decimal below.
    // The 48 derived message-schedule words and the 64 compression rounds are now
    // ROLLED into condition-driven loops (R8) over a Copy `array<u32, 64>`
    // accumulator updated by index with `array_set` (R9), so each function's stack
    // frame is small and the whole hash compiles native — eval == native on every
    // word (the Milestone V3.3 acceptance: sha256.cdb compiles native).
    const WORDS: [&str; 8] = [
        "3128432319",
        "2399260650",
        "1094795486",
        "1571693091",
        "2953011619",
        "2518121116",
        "3021012833",
        "4060091821",
    ];
    let temp = tempdir().unwrap();
    let db = temp.path().join("sha256.sqlite");
    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/v3/sha256.cdb"]);
    run(&["verify", path(&db)]);
    for (i, word) in WORDS.iter().enumerate() {
        assert_eq!(
            run(&["eval", path(&db), &format!("digest_{i}")]).trim(),
            *word,
            "sha256(abc) word {i}"
        );
    }

    if can_build_default_native_target() {
        for (i, word) in WORDS.iter().enumerate() {
            let created = parse_json(&run(&[
                "create-test",
                path(&db),
                &format!("digest_{i}_native"),
                "--entry",
                &format!("digest_{i}"),
                "--expect-int",
                &format!("u32:{word}"),
                "--native-required",
                "--json",
            ]));
            assert_eq!(created["status"], "applied", "create-test digest_{i}");
        }
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed", "sha256 native run: {report}");
        assert_eq!(
            report["native_mismatches"], 0,
            "sha256 native digest matches the reference on every word: {report}"
        );
        assert_eq!(report["unsupported"], 0);
    }
}
