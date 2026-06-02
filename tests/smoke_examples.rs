use std::path::Path;
use std::process::Command as ProcessCommand;

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

fn run_failure(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn parse_line_value<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .unwrap_or_else(|| panic!("missing line prefix {key:?} in:\n{text}"))
        .trim()
}

fn branch_pointer(db: &Path) -> JsonValue {
    let branches = parse_json(&run(&["branches", path(db), "--json"]));
    assert_eq!(branches["schema"], "codedb/branches/v1");
    branches["branches"][0].clone()
}

#[test]
fn readme_quickstart_and_cookbook_smoke() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("demo.codedb.sqlite");
    let rebuilt_db = temp.path().join("rebuilt.codedb.sqlite");
    let projection = temp.path().join("projection.cdb");
    let c_projection = temp.path().join("projection.c");
    let ir = temp.path().join("main.ir.json");
    let object = temp.path().join("main.o");
    let link_plan = temp.path().join("main.link.json");
    let history = temp.path().join("history.ndjson");

    run(&["init", path(&db)]);
    run(&["import", path(&db), "examples/shop.cdb"]);

    assert_eq!(run(&["eval", path(&db), "main"]), "120\n");
    assert!(run(&["callers", path(&db), "tax"]).contains("total"));

    let show_tax = run(&["show", path(&db), "tax"]);
    let tax_internal_abi = parse_line_value(&show_tax, "internal_abi_symbol ").to_string();
    assert!(tax_internal_abi.starts_with("codedb_"));
    assert!(show_tax.contains("source fn tax(subtotal: i64) -> i64"));

    let list = parse_json(&run(&["list", path(&db), "--json"]));
    assert_eq!(list["branch"], "main");
    assert_eq!(list["symbols"].as_array().unwrap().len(), 3);
    assert!(list["symbols"].as_array().unwrap().iter().any(|symbol| {
        symbol["name"] == "tax" && symbol["signature"] == "(subtotal: i64) -> i64"
    }));

    let history_json = parse_json(&run(&["history", path(&db), "--json"]));
    assert_eq!(history_json["branch"], "main");
    assert_eq!(history_json["migrations"].as_array().unwrap().len(), 3);
    assert_eq!(
        history_json["migrations"][0]["operation_kind"],
        "create_function"
    );

    let branches = parse_json(&run(&["branches", path(&db), "--json"]));
    assert_eq!(branches["schema"], "codedb/branches/v1");
    assert_eq!(branches["branches"][0]["name"], "main");

    let export_map = run(&["export-map", path(&db)]);
    assert!(export_map.contains("main.tax"));
    assert!(export_map.contains(&tax_internal_abi));
    assert!(export_map.contains("exported_abi_symbols none"));

    bin()
        .args(["verify", path(&db)])
        .assert()
        .success()
        .stdout("verify ok\n");

    let set_export = run(&["set-export", path(&db), "tax", "public_tax"]);
    assert!(set_export.contains("applied set_export main.tax as public_tax"));
    assert!(set_export.contains("build_impact relink_only"));
    assert!(run(&["export-map", path(&db)]).contains("public_tax"));

    let rename = run(&["rename", path(&db), "tax", "vat"]);
    assert!(rename.contains("applied rename_symbol main.tax -> main.vat"));
    assert!(rename.contains("build_impact metadata_only"));
    let rename_old_root = parse_line_value(&rename, "old_root ");
    let rename_new_root = parse_line_value(&rename, "new_root ");
    let rename_diff = parse_json(&run(&[
        "diff",
        path(&db),
        rename_old_root,
        rename_new_root,
        "--json",
    ]));
    assert_eq!(rename_diff["build_impact"]["kind"], "metadata_only");
    assert_eq!(rename_diff["changes"][0]["kind"], "symbol_renamed");

    let show_vat = run(&["show", path(&db), "vat"]);
    assert_eq!(
        parse_line_value(&show_vat, "internal_abi_symbol "),
        tax_internal_abi
    );
    assert!(show_vat.contains("exported_abi_symbols public_tax"));

    let replace = run(&["replace-body", path(&db), "vat", "subtotal * 18 / 100"]);
    assert!(replace.contains("applied replace_function_body main.vat"));
    assert!(replace.contains("build_impact recompile_symbols"));
    assert_eq!(run(&["eval", path(&db), "main"]), "118\n");
    let replace_old_root = parse_line_value(&replace, "old_root ");
    let replace_new_root = parse_line_value(&replace, "new_root ");
    let replace_diff = parse_json(&run(&[
        "diff",
        path(&db),
        replace_old_root,
        replace_new_root,
        "--json",
    ]));
    assert_eq!(replace_diff["build_impact"]["kind"], "recompile_symbols");
    assert_eq!(replace_diff["changes"][0]["kind"], "implementation_changed");

    let create_alias = run(&["create-alias", path(&db), "vat", "sales_tax"]);
    assert!(create_alias.contains("applied create_alias main.vat as main.sales_tax"));
    assert!(run(&["show", path(&db), "sales_tax"]).contains("name main.vat"));
    let remove_alias = run(&["remove-alias", path(&db), "vat", "sales_tax"]);
    assert!(remove_alias.contains("applied remove_alias main.vat as main.sales_tax"));
    assert!(run_failure(&["show", path(&db), "sales_tax"]).contains("unknown name"));

    let remove_export = run(&["remove-export", path(&db), "vat", "public_tax"]);
    assert!(remove_export.contains("applied remove_export main.vat as public_tax"));
    assert!(!run(&["export-map", path(&db)]).contains("public_tax"));

    run(&[
        "export",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&projection),
    ]);
    let source = std::fs::read_to_string(&projection).unwrap();
    assert!(source.contains("fn vat(subtotal: i64) -> i64 = subtotal * 18 / 100"));
    assert!(source.contains("subtotal + vat(subtotal)"));

    run(&["emit-c", path(&db), "main", "--out", path(&c_projection)]);
    let c_source = std::fs::read_to_string(&c_projection).unwrap();
    assert!(c_source.contains("long codedb_vat(long subtotal)"));
    assert!(c_source.contains("return subtotal + codedb_vat(subtotal);"));
    for forbidden in ["malloc", "free", "printf", "pthread_"] {
        assert!(!c_source.contains(forbidden));
    }

    run(&["emit-ir", path(&db), "main", "--out", path(&ir)]);
    let ir_json = parse_json(&std::fs::read_to_string(&ir).unwrap());
    assert_eq!(ir_json["schema"], "codedb/lowered-ir-inspection/v1");
    assert_eq!(ir_json["ir"]["schema"], "codedb/lowered-function-ir/v1");

    run(&[
        "emit-object",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&object),
    ]);
    let object_bytes = std::fs::read(&object).unwrap();
    assert_eq!(&object_bytes[..4], b"\x7fELF");

    run(&[
        "link-native",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&link_plan),
    ]);
    let link_json = parse_json(&std::fs::read_to_string(&link_plan).unwrap());
    assert_eq!(link_json["schema"], "codedb/link-plan/v1");
    assert_eq!(link_json["target_triple"], codedb::LINUX_X86_64_TARGET);
    assert_eq!(link_json["objects"].as_array().unwrap().len(), 3);

    let build_plan = parse_json(&run(&[
        "build-plan",
        path(&db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--json",
    ]));
    assert_eq!(build_plan["schema"], "codedb/native-build-plan/v1");
    assert_eq!(build_plan["target_triple"], codedb::LINUX_X86_64_TARGET);
    assert!(
        build_plan["link_plan_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );

    smoke_host_native_build_if_supported(&db, temp.path(), 118);

    let source_branch = branch_pointer(&db);
    run(&[
        "export-history",
        path(&db),
        "--branch",
        "main",
        "--out",
        path(&history),
    ]);
    assert!(
        std::fs::read_to_string(&history)
            .unwrap()
            .lines()
            .all(|line| parse_json(line).is_object())
    );

    run(&["init", path(&rebuilt_db)]);
    let import_report = run(&["import-history", path(&rebuilt_db), path(&history)]);
    assert!(import_report.contains("imported history"));
    assert_eq!(branch_pointer(&rebuilt_db), source_branch);
    assert_eq!(run(&["eval", path(&rebuilt_db), "main"]), "118\n");
    bin()
        .args(["verify", path(&rebuilt_db)])
        .assert()
        .success()
        .stdout("verify ok\n");

    bin()
        .args(["verify", path(&db)])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn readme_structural_apply_smoke() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("apply-demo.codedb.sqlite");

    run(&["init", path(&db)]);
    let applied = parse_json(&run(&[
        "apply",
        path(&db),
        "--json",
        "examples/shop.apply.json",
    ]));
    assert_eq!(applied["schema"], "codedb/apply-result/v1");
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["committed"], true);
    assert_eq!(applied["operation_count"], 3);
    assert_eq!(applied["processed_operation_count"], 3);
    assert_eq!(applied["applied_operation_count"], 3);
    assert_eq!(applied["results"][0]["summary"]["kind"], "create_function");

    assert_eq!(run(&["eval", path(&db), "main"]), "120\n");

    let list = parse_json(&run(&["list", path(&db), "--json"]));
    assert_eq!(list["root_hash"], applied["new_root_hash"]);
    assert_eq!(list["symbols"].as_array().unwrap().len(), 3);

    let history = parse_json(&run(&["history", path(&db), "--json"]));
    assert_eq!(history["history_hash"], applied["history_hash"]);
    assert_eq!(history["migrations"].as_array().unwrap().len(), 3);

    bin()
        .args(["verify", path(&db)])
        .assert()
        .success()
        .stdout("verify ok\n");
}

fn smoke_host_native_build_if_supported(db: &Path, dir: &Path, expected_exit: i32) {
    if !can_build_default_native_target() {
        eprintln!(
            "skipping codedb build smoke: default target {} is not linkable on this host",
            codedb::DEFAULT_NATIVE_TARGET
        );
        return;
    }
    if !has_cc() {
        eprintln!("skipping codedb build smoke: cc linker is not available");
        return;
    }

    let exe = dir.join("demo-native");
    run(&["build", path(db), "main", "--out", path(&exe)]);
    let status = ProcessCommand::new(&exe)
        .status()
        .expect("run smoke native executable");
    assert_eq!(status.code(), Some(expected_exit));
}

fn has_cc() -> bool {
    ProcessCommand::new("cc")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn can_build_default_native_target() -> bool {
    (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64")
        || (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
}
