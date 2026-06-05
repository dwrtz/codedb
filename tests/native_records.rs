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

fn read_json(path: &Path) -> JsonValue {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn records_compile_end_to_end_with_params_returns_references_and_native_tests() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-records.sqlite");
    let source = temp.path().join("native-records.cdb");
    let main_ir_path = temp.path().join("main.ir.json");
    let make_big_ir_path = temp.path().join("make-big.ir.json");
    let object_path = temp.path().join("make-big.o");

    std::fs::write(
        &source,
        r#"
record Pair {
  left: i64
  right: i64
}

record Big {
  a: i64
  b: i64
  c: i64
  d: i64
}

record Line {
  price_cents: i64
  qty: i64
}

record LineView<'a> {
  line: &'a Line
}

fn sum_pair(pair: Pair) -> i64 = pair.left + pair.right

fn make_pair() -> Pair =
  let pair: Pair = { left: 10, right: 7 } in
  pair

fn sum_big(big: Big) -> i64 = big.a + big.b + big.c + big.d

fn make_big() -> Big =
  let big: Big = { a: 1, b: 2, c: 3, d: 4 } in
  big

fn line_total<'a>(view: LineView<'a>) -> i64 =
  view.line.price_cents * view.line.qty

fn refs_main<'a>() -> i64 =
  let line: Line = { price_cents: 25, qty: 4 } in
  let view: LineView<'a> = { line: &'a line } in
  line_total(view)

fn main() -> i64 = sum_pair({ left: 2, right: 3 }) + sum_pair(make_pair()) + sum_big(make_big()) + refs_main()
"#,
    )
    .unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&source)]);
    assert_eq!(run(&["eval", path(&db), "main"]).trim(), "132");

    run(&["emit-ir", path(&db), "main", "--out", path(&main_ir_path)]);
    run(&[
        "emit-ir",
        path(&db),
        "make_big",
        "--out",
        path(&make_big_ir_path),
    ]);
    let main_ir = read_json(&main_ir_path);
    let make_big_ir = read_json(&make_big_ir_path);
    assert!(
        main_ir["ir"]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["op"] == "call" && op.get("return_address").is_some())
    );
    let big_return_layout = make_big_ir["ir"]["type_layouts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|layout| layout["type_hash"] == make_big_ir["ir"]["return_type_hash"])
        .unwrap();
    assert_eq!(big_return_layout["kind"], "record");
    assert_eq!(big_return_layout["abi"]["pass"], "by_indirect");
    assert_eq!(big_return_layout["abi"]["return"], "hidden_return_slot");

    run(&[
        "emit-object",
        path(&db),
        "make_big",
        "--target",
        codedb::DEFAULT_NATIVE_TARGET,
        "--out",
        path(&object_path),
    ]);
    let object_bytes = std::fs::read(&object_path).unwrap();
    if codedb::DEFAULT_NATIVE_TARGET == codedb::LINUX_X86_64_TARGET {
        assert_eq!(&object_bytes[..4], b"\x7fELF");
    } else {
        assert_eq!(&object_bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    }
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            "native_records",
            "--entry",
            "main",
            "--expect-i64",
            "132",
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied");

        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed");
        assert_eq!(report["passed"], 1);
        assert_eq!(report["unsupported"], 0);
        assert_eq!(report["native_mismatches"], 0);
        assert_eq!(report["tests"][0]["native"]["status"], "passed");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"],
            json!({"kind": "i64", "value": "132"})
        );
    }
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}
