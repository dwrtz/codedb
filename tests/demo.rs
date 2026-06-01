use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
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

fn parse_line_value<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .unwrap_or_else(|| panic!("missing line prefix {key:?} in:\n{text}"))
        .trim()
}

#[test]
fn native_object_backend_emits_elf_and_reuses_cache_across_rename() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native.sqlite");
    let tax_obj = temp.path().join("tax.o");
    let total_obj = temp.path().join("total.o");
    let main_obj = temp.path().join("main.o");
    let total_after_rename_obj = temp.path().join("total-after-rename.o");
    let vat_obj = temp.path().join("vat.o");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    let show_tax = run(&["show", db.to_str().unwrap(), "tax"]);
    let tax_internal_abi = parse_line_value(&show_tax, "internal_abi_symbol ").to_string();
    let tax_definition = parse_line_value(&show_tax, "definition ").to_string();
    let show_total = run(&["show", db.to_str().unwrap(), "total"]);
    let total_definition = parse_line_value(&show_total, "definition ").to_string();
    let show_main = run(&["show", db.to_str().unwrap(), "main"]);
    let main_internal_abi = parse_line_value(&show_main, "internal_abi_symbol ").to_string();

    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        tax_obj.to_str().unwrap(),
    ]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        total_obj.to_str().unwrap(),
    ]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        main_obj.to_str().unwrap(),
    ]);

    let tax_bytes = std::fs::read(&tax_obj).unwrap();
    let total_bytes = std::fs::read(&total_obj).unwrap();
    assert_eq!(&tax_bytes[..4], b"\x7fELF");
    assert!(bytes_contain(&tax_bytes, tax_internal_abi.as_bytes()));
    assert!(!bytes_contain(&tax_bytes, b"tax"));
    assert!(!bytes_contain(&total_bytes, b"total"));
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 3);

    let (tax_cache_json, tax_cache_bytes) = object_cache_entry_by_definition(&db, &tax_definition);
    assert_eq!(tax_cache_bytes, tax_bytes);
    assert_eq!(tax_cache_json["content_kind"], "bytes");
    assert_eq!(
        tax_cache_json["metadata"]["object_format"],
        "elf64-x86-64-relocatable"
    );
    assert_eq!(
        tax_cache_json["metadata"]["target_triple"],
        codedb::LINUX_X86_64_TARGET
    );
    assert_eq!(
        tax_cache_json["metadata"]["defined_symbols"][0],
        tax_internal_abi
    );

    let (total_cache_json, _) = object_cache_entry_by_definition(&db, &total_definition);
    assert_eq!(
        total_cache_json["metadata"]["relocations"][0]["target_abi_symbol"],
        tax_internal_abi
    );
    assert_eq!(
        total_cache_json["metadata"]["relocations"][0]["kind"],
        "R_X86_64_PLT32"
    );

    link_and_run_native_if_linux(
        temp.path(),
        &[&tax_obj, &total_obj, &main_obj],
        &main_internal_abi,
        120,
    );

    run(&["rename", db.to_str().unwrap(), "tax", "vat"]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        total_after_rename_obj.to_str().unwrap(),
    ]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "vat",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        vat_obj.to_str().unwrap(),
    ]);
    assert_eq!(std::fs::read(&total_after_rename_obj).unwrap(), total_bytes);
    assert_eq!(std::fs::read(&vat_obj).unwrap(), tax_bytes);
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 3);

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn native_object_backend_reuses_unchanged_objects_after_body_change() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-body-change.sqlite");
    let tax_before = temp.path().join("tax-before.o");
    let tax_after = temp.path().join("tax-after.o");
    let total_before = temp.path().join("total-before.o");
    let total_after = temp.path().join("total-after.o");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        tax_before.to_str().unwrap(),
    ]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        total_before.to_str().unwrap(),
    ]);
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 2);

    run(&[
        "replace-body",
        db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "total",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        total_after.to_str().unwrap(),
    ]);
    assert_eq!(
        std::fs::read(&total_after).unwrap(),
        std::fs::read(&total_before).unwrap()
    );
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 2);

    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        tax_after.to_str().unwrap(),
    ]);
    assert_ne!(
        std::fs::read(&tax_after).unwrap(),
        std::fs::read(&tax_before).unwrap()
    );
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 3);
}

#[test]
fn native_object_backend_handles_bool_calls_and_conditionals() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-booleans.sqlite");
    let is_large_obj = temp.path().join("is-large.o");
    let fee_obj = temp.path().join("fee.o");
    let main_obj = temp.path().join("main.o");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/booleans.cdb"]);

    for (name, out) in [
        ("is_large", &is_large_obj),
        ("fee", &fee_obj),
        ("main", &main_obj),
    ] {
        run(&[
            "emit-object",
            db.to_str().unwrap(),
            name,
            "--target",
            codedb::LINUX_X86_64_TARGET,
            "--out",
            out.to_str().unwrap(),
        ]);
        let bytes = std::fs::read(out).unwrap();
        assert_eq!(&bytes[..4], b"\x7fELF");
    }

    let show_main = run(&["show", db.to_str().unwrap(), "main"]);
    let main_internal_abi = parse_line_value(&show_main, "internal_abi_symbol ").to_string();
    link_and_run_native_if_linux(
        temp.path(),
        &[&is_large_obj, &fee_obj, &main_obj],
        &main_internal_abi,
        5,
    );
}

#[test]
fn native_object_backend_links_default_target_on_apple_silicon() {
    if !is_apple_silicon() {
        return;
    }

    let temp = tempdir().unwrap();
    let db = temp.path().join("native-apple.sqlite");
    let tax_obj = temp.path().join("tax.o");
    let discount_obj = temp.path().join("discount.o");
    let total_obj = temp.path().join("total.o");
    let main_obj = temp.path().join("main.o");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/discount.cdb"]);

    for (name, out) in [
        ("tax", &tax_obj),
        ("discount", &discount_obj),
        ("total", &total_obj),
        ("main", &main_obj),
    ] {
        run(&[
            "emit-object",
            db.to_str().unwrap(),
            name,
            "--out",
            out.to_str().unwrap(),
        ]);
        let bytes = std::fs::read(out).unwrap();
        assert_eq!(&bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    }

    let show_main = run(&["show", db.to_str().unwrap(), "main"]);
    let main_internal_abi = parse_line_value(&show_main, "internal_abi_symbol ").to_string();
    let main_definition = parse_line_value(&show_main, "definition ").to_string();
    let main_bytes = std::fs::read(&main_obj).unwrap();
    assert!(bytes_contain(
        &main_bytes,
        format!("_{main_internal_abi}").as_bytes()
    ));

    let (main_cache_json, main_cache_bytes) =
        object_cache_entry_by_definition(&db, &main_definition);
    assert_eq!(main_cache_bytes, main_bytes);
    assert_eq!(
        main_cache_json["metadata"]["object_format"],
        "macho64-arm64-relocatable"
    );
    assert_eq!(
        main_cache_json["metadata"]["target_triple"],
        codedb::APPLE_ARM64_TARGET
    );
    assert_eq!(
        main_cache_json["metadata"]["defined_symbols"][0],
        main_internal_abi
    );

    link_and_run_native_if_apple_silicon(
        temp.path(),
        &[&tax_obj, &discount_obj, &total_obj, &main_obj],
        &main_internal_abi,
        165,
    );
}

#[test]
fn native_link_plan_is_deterministic_and_reused_across_rename() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-link-plan.sqlite");
    let plan_before = temp.path().join("before.link.json");
    let plan_again = temp.path().join("again.link.json");
    let plan_after_rename = temp.path().join("after-rename.link.json");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_before.to_str().unwrap(),
    ]);
    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_again.to_str().unwrap(),
    ]);
    let before = std::fs::read_to_string(&plan_before).unwrap();
    let again = std::fs::read_to_string(&plan_again).unwrap();
    assert_eq!(before, again);
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 3);
    assert_eq!(cache_row_count_by_kind(&db, "link_plan"), 1);
    assert!(!before.contains("tax"));
    assert!(!before.contains("total"));

    let plan_json: JsonValue = serde_json::from_str(&before).unwrap();
    assert_eq!(plan_json["schema"], "codedb/link-plan/v1");
    assert_eq!(plan_json["target_triple"], codedb::LINUX_X86_64_TARGET);
    assert_eq!(plan_json["objects"].as_array().unwrap().len(), 3);
    assert_eq!(plan_json["external_symbols"].as_array().unwrap().len(), 0);

    run(&["rename", db.to_str().unwrap(), "tax", "vat"]);
    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_after_rename.to_str().unwrap(),
    ]);
    assert_eq!(std::fs::read_to_string(&plan_after_rename).unwrap(), before);
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 3);
    assert_eq!(cache_row_count_by_kind(&db, "link_plan"), 1);
}

#[test]
fn link_plan_cache_key_records_object_dependencies_and_duplicate_relocations() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("native-link-deps.sqlite");
    let source = temp.path().join("two-calls.cdb");
    let plan_path = temp.path().join("two-calls.link.json");
    let plan_again_path = temp.path().join("two-calls-again.link.json");

    std::fs::write(
        &source,
        "fn inc(x: i64) -> i64 = x + 1\n\nfn main() -> i64 = inc(1) + inc(2)\n",
    )
    .unwrap();

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), source.to_str().unwrap()]);
    let main_symbol =
        parse_line_value(&run(&["show", db.to_str().unwrap(), "main"]), "symbol ").to_string();

    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_path.to_str().unwrap(),
    ]);
    let plan_text = std::fs::read_to_string(&plan_path).unwrap();
    let plan_json: JsonValue = serde_json::from_str(&plan_text).unwrap();
    let main_object = plan_json["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["symbol_hash"] == main_symbol)
        .expect("main object in link plan");
    let relocations = main_object["relocations"].as_array().unwrap();
    assert_eq!(relocations.len(), 2);
    assert!(relocations.iter().all(|relocation| {
        relocation["kind"] == "R_X86_64_PLT32" && relocation["offset"].as_u64().is_some()
    }));

    let key_jsons = cache_key_json_values_by_kind(&db, "link_plan");
    assert_eq!(key_jsons.len(), 1);
    let mut key_dependencies = key_jsons[0]["dependency_implementation_hashes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let mut plan_objects = plan_json["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|object| object["object_artifact_hash"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    key_dependencies.sort();
    plan_objects.sort();
    assert_eq!(key_dependencies, plan_objects);

    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_again_path.to_str().unwrap(),
    ]);
    assert_eq!(
        std::fs::read_to_string(&plan_again_path).unwrap(),
        plan_text
    );
    assert_eq!(cache_row_count_by_kind(&db, "link_plan"), 1);
}

#[test]
fn native_build_emits_apple_executable_and_caches_it() {
    if !is_apple_silicon() {
        return;
    }

    let temp = tempdir().unwrap();
    let db = temp.path().join("native-build.sqlite");
    let exe = temp.path().join("discount");
    let exe_again = temp.path().join("discount-again");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/discount.cdb"]);
    run(&[
        "build",
        db.to_str().unwrap(),
        "main",
        "--out",
        exe.to_str().unwrap(),
    ]);

    let bytes = std::fs::read(&exe).unwrap();
    assert_eq!(&bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    let status = StdCommand::new(&exe)
        .status()
        .expect("run built executable");
    assert_eq!(status.code(), Some(165));
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 4);
    assert_eq!(cache_row_count_by_kind(&db, "link_plan"), 1);
    assert_eq!(cache_row_count_by_kind(&db, "executable"), 1);

    run(&[
        "build",
        db.to_str().unwrap(),
        "main",
        "--out",
        exe_again.to_str().unwrap(),
    ]);
    assert_eq!(std::fs::read(&exe_again).unwrap(), bytes);
    assert_eq!(cache_row_count_by_kind(&db, "executable"), 1);
}

#[test]
fn interface_cache_is_per_symbol_even_when_signatures_match() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("interfaces.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    let interface_rows = cache_json_values_by_kind(&db, "interface_hash");
    let symbols = interface_rows
        .iter()
        .map(|value| {
            value["metadata"]["symbol_hash"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<std::collections::BTreeSet<_>>();
    let signatures = interface_rows
        .iter()
        .map(|value| {
            value["metadata"]["signature_hash"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(interface_rows.len(), 3);
    assert_eq!(symbols.len(), 3);
    assert!(signatures.len() < symbols.len());
}

#[test]
fn shop_demo_flow_preserves_symbol_identity_across_rename() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("demo.sqlite");
    let projection = temp.path().join("projection.cdb");
    let c_file = temp.path().join("projection.c");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    bin()
        .args(["eval", db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("120\n");

    bin()
        .args(["callers", db.to_str().unwrap(), "tax"])
        .assert()
        .success()
        .stdout(predicate::str::contains("total"));

    let show_tax = run(&["show", db.to_str().unwrap(), "tax"]);
    let tax_internal_abi = parse_line_value(&show_tax, "internal_abi_symbol ");
    assert!(tax_internal_abi.starts_with("codedb_"));
    assert!(!tax_internal_abi.contains("tax"));
    assert!(show_tax.contains("exported_abi_symbols none"));

    let rename = run(&["rename", db.to_str().unwrap(), "tax", "vat"]);
    let old_root = parse_line_value(&rename, "old_root ");
    let new_root = parse_line_value(&rename, "new_root ");
    assert!(rename.contains("build_impact metadata_only"));
    assert!(rename.contains("recompile none"));
    assert!(rename.contains("relink false"));

    let show_vat = run(&["show", db.to_str().unwrap(), "vat"]);
    assert_eq!(
        parse_line_value(&show_vat, "internal_abi_symbol "),
        tax_internal_abi
    );
    assert!(show_vat.contains("exported_abi_symbols none"));

    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root]);
    assert!(diff.contains("symbol_renamed"));
    assert!(diff.contains("main.tax -> main.vat"));
    assert!(diff.contains("function body hash: unchanged"));
    assert!(diff.contains("Incremental build impact"));
    assert!(diff.contains("build_impact metadata_only"));

    let branch_before_retry = branch_state(&db);
    let migrations_before_retry = row_count(&db, "migrations");
    let retry = run(&["rename", db.to_str().unwrap(), "tax", "vat"]);
    assert!(retry.contains("already_applied rename_symbol main.tax -> main.vat"));
    assert_eq!(branch_state(&db), branch_before_retry);
    assert_eq!(row_count(&db, "migrations"), migrations_before_retry);

    run(&[
        "export",
        db.to_str().unwrap(),
        "--branch",
        "main",
        "--out",
        projection.to_str().unwrap(),
    ]);
    let source = std::fs::read_to_string(&projection).unwrap();
    assert!(source.contains("fn vat(subtotal: i64) -> i64"));
    assert!(source.contains("subtotal + vat(subtotal)"));
    assert!(!source.contains("tax("));

    run(&[
        "emit-c",
        db.to_str().unwrap(),
        "main",
        "--out",
        c_file.to_str().unwrap(),
    ]);
    let c_source = std::fs::read_to_string(&c_file).unwrap();
    assert!(c_source.contains("long codedb_vat(long subtotal)"));
    assert!(c_source.contains("return subtotal + codedb_vat(subtotal);"));
    for forbidden in ["malloc", "free", "printf", "pthread_"] {
        assert!(!c_source.contains(forbidden));
    }

    let cache_rows = cache_rows(&db);
    assert!(cache_rows.contains(&(
        "projection".to_string(),
        "canonical_source".to_string(),
        "canonical_source".to_string()
    )));
    assert!(cache_rows.contains(&(
        "projection".to_string(),
        "c_source".to_string(),
        "c_projection".to_string()
    )));
    assert!(
        cache_rows
            .iter()
            .all(|(_, _, artifact_kind)| artifact_kind != "rendered_source")
    );

    compile_and_run_c_if_available(&temp.path().join("projection.c"));

    bin()
        .args(["replay", db.to_str().unwrap(), "--from-genesis"])
        .assert()
        .success()
        .stdout(predicate::str::contains("replay ok"));

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn export_map_changes_are_explicit_relink_only_operations() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("exports.sqlite");
    let c_file = temp.path().join("exports_projection.c");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    let show_tax = run(&["show", db.to_str().unwrap(), "tax"]);
    let tax_internal_abi = parse_line_value(&show_tax, "internal_abi_symbol ");

    let set_export = run(&["set-export", db.to_str().unwrap(), "tax", "public_tax"]);
    let old_root = parse_line_value(&set_export, "old_root ");
    let new_root = parse_line_value(&set_export, "new_root ");
    assert!(set_export.contains("applied set_export main.tax as public_tax"));
    assert!(set_export.contains("semantic_impact export_set"));
    assert!(set_export.contains("build_impact relink_only"));
    assert!(set_export.contains("recompile none"));
    assert!(set_export.contains("relink true"));
    assert!(set_export.contains("link_plan"));
    assert!(set_export.contains("export_map_changed"));

    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root]);
    assert!(diff.contains("export_added"));
    assert!(diff.contains("exported_abi_symbol: public_tax"));
    assert!(diff.contains("compile impact: relink_only"));
    assert!(diff.contains("build_impact relink_only"));

    let show_exported_tax = run(&["show", db.to_str().unwrap(), "tax"]);
    assert_eq!(
        parse_line_value(&show_exported_tax, "internal_abi_symbol "),
        tax_internal_abi
    );
    assert!(show_exported_tax.contains("exported_abi_symbols public_tax"));

    let branch_before_retry = branch_state(&db);
    let counts_before_retry = mutation_guard_counts(&db);
    let retry = run(&["set-export", db.to_str().unwrap(), "tax", "public_tax"]);
    assert!(retry.contains("already_applied set_export main.tax as public_tax"));
    assert_eq!(branch_state(&db), branch_before_retry);
    assert_eq!(mutation_guard_counts(&db), counts_before_retry);

    let rename = run(&["rename", db.to_str().unwrap(), "tax", "vat"]);
    assert!(rename.contains("build_impact metadata_only"));
    assert!(rename.contains("relink false"));

    let show_vat = run(&["show", db.to_str().unwrap(), "vat"]);
    assert_eq!(
        parse_line_value(&show_vat, "internal_abi_symbol "),
        tax_internal_abi
    );
    assert!(show_vat.contains("exported_abi_symbols public_tax"));

    let export_map = run(&["export-map", db.to_str().unwrap()]);
    assert!(export_map.contains("main.vat"));
    assert!(export_map.contains(tax_internal_abi));
    assert!(export_map.contains("exported_abi_symbols public_tax"));

    run(&[
        "emit-c",
        db.to_str().unwrap(),
        "main",
        "--out",
        c_file.to_str().unwrap(),
    ]);
    let c_source = std::fs::read_to_string(&c_file).unwrap();
    assert!(c_source.contains("long codedb_vat(long subtotal)"));
    assert!(c_source.contains("return subtotal + codedb_vat(subtotal);"));
    assert!(!c_source.contains("public_tax"));

    let remove_export = run(&["remove-export", db.to_str().unwrap(), "vat", "public_tax"]);
    assert!(remove_export.contains("applied remove_export main.vat as public_tax"));
    assert!(remove_export.contains("semantic_impact export_removed"));
    assert!(remove_export.contains("build_impact relink_only"));

    let show_unexported_vat = run(&["show", db.to_str().unwrap(), "vat"]);
    assert!(show_unexported_vat.contains("exported_abi_symbols none"));

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn export_map_changes_do_not_stale_native_object_metadata() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("export-object-metadata.sqlite");
    let tax_before = temp.path().join("tax-before.o");
    let tax_after = temp.path().join("tax-after.o");
    let plan_path = temp.path().join("exports.link.json");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let tax_definition =
        parse_line_value(&run(&["show", db.to_str().unwrap(), "tax"]), "definition ").to_string();

    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        tax_before.to_str().unwrap(),
    ]);
    run(&["set-export", db.to_str().unwrap(), "tax", "public_tax"]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        tax_after.to_str().unwrap(),
    ]);

    assert_eq!(
        std::fs::read(&tax_before).unwrap(),
        std::fs::read(&tax_after).unwrap()
    );
    assert_eq!(cache_row_count_by_kind(&db, "object_file"), 1);
    let (object_cache_json, _) = object_cache_entry_by_definition(&db, &tax_definition);
    let object_cache_text = serde_json::to_string(&object_cache_json).unwrap();
    assert!(!object_cache_text.contains("exported_abi_names"));
    assert!(!object_cache_text.contains("public_tax"));

    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_path.to_str().unwrap(),
    ]);
    let plan_text = std::fs::read_to_string(&plan_path).unwrap();
    assert!(plan_text.contains("public_tax"));
}

#[test]
fn lowered_ir_uses_symbol_hash_calls_and_reuses_cache_across_rename() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("lowered-ir.sqlite");
    let tax_ir = temp.path().join("tax.ir.json");
    let total_ir_before = temp.path().join("total-before.ir.json");
    let total_ir_after = temp.path().join("total-after.ir.json");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    run(&[
        "emit-ir",
        db.to_str().unwrap(),
        "tax",
        "--out",
        tax_ir.to_str().unwrap(),
    ]);
    let tax_ir_text = std::fs::read_to_string(&tax_ir).unwrap();
    let tax_ir_json: JsonValue = serde_json::from_str(&tax_ir_text).unwrap();
    assert_eq!(tax_ir_json["ir"]["schema"], "codedb/lowered-function-ir/v1");
    assert!(tax_ir_text.contains("division_by_zero"));
    assert!(tax_ir_text.contains("\"op\": \"return\""));

    run(&[
        "emit-ir",
        db.to_str().unwrap(),
        "total",
        "--out",
        total_ir_before.to_str().unwrap(),
    ]);
    let before_text = std::fs::read_to_string(&total_ir_before).unwrap();
    let before: JsonValue = serde_json::from_str(&before_text).unwrap();
    let before_hash = before["lowered_ir_hash"].as_str().unwrap().to_string();
    assert!(before_text.contains("target_symbol_hash"));
    assert!(!before_text.contains("tax"));
    assert!(!before_text.contains("total"));
    assert!(!before_text.contains("vat"));

    let lowered_cache_rows = cache_row_count_by_kind(&db, "lowered_ir");
    assert_eq!(lowered_cache_rows, 2);

    run(&["rename", db.to_str().unwrap(), "tax", "vat"]);
    run(&[
        "emit-ir",
        db.to_str().unwrap(),
        "total",
        "--out",
        total_ir_after.to_str().unwrap(),
    ]);
    let after_text = std::fs::read_to_string(&total_ir_after).unwrap();
    let after: JsonValue = serde_json::from_str(&after_text).unwrap();
    assert_eq!(after["lowered_ir_hash"].as_str().unwrap(), before_hash);
    assert_eq!(
        cache_row_count_by_kind(&db, "lowered_ir"),
        lowered_cache_rows
    );
    assert!(after_text.contains("target_symbol_hash"));
    assert!(!after_text.contains("tax"));
    assert!(!after_text.contains("total"));
    assert!(!after_text.contains("vat"));

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn replace_body_updates_only_implementation_and_literal_diff() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("replace.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let replace = run(&[
        "replace-body",
        db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    let old_root = parse_line_value(&replace, "old_root ");
    let new_root = parse_line_value(&replace, "new_root ");
    assert!(replace.contains("build_impact recompile_symbols"));
    assert!(replace.contains("relink true"));
    let recompile = parse_line_value(&replace, "recompile ");
    assert!(recompile.starts_with("sha256:"));
    assert!(!recompile.contains(','));

    bin()
        .args(["eval", db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("118\n");

    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root]);
    assert!(diff.contains("implementation_changed"));
    assert!(diff.contains("signature: unchanged"));
    assert!(diff.contains("literal_changed: 20 -> 18"));
    assert!(diff.contains("compile impact: recompile_symbols"));

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn build_impact_is_available_as_json() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("json.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let rename = run(&["rename", db.to_str().unwrap(), "tax", "vat", "--json"]);
    let value: JsonValue = serde_json::from_str(&rename).unwrap();

    assert_eq!(value["status"], "applied");
    assert_eq!(value["summary"]["build_impact"]["kind"], "metadata_only");
    assert_eq!(value["summary"]["build_impact"]["relink"], false);
    assert_eq!(
        value["summary"]["build_impact"]["recompile"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let old_root = value["old_root_hash"].as_str().unwrap();
    let new_root = value["new_root_hash"].as_str().unwrap();
    let diff = run(&["diff", db.to_str().unwrap(), old_root, new_root, "--json"]);
    let diff: JsonValue = serde_json::from_str(&diff).unwrap();
    assert_eq!(diff["build_impact"]["kind"], "metadata_only");
    assert_eq!(diff["changes"][0]["kind"], "symbol_renamed");
}

#[test]
fn conditionals_and_booleans_import_and_evaluate() {
    let temp = tempdir().unwrap();

    let discount_db = temp.path().join("discount.sqlite");
    run(&["init", discount_db.to_str().unwrap()]);
    run(&[
        "import",
        discount_db.to_str().unwrap(),
        "examples/discount.cdb",
    ]);
    bin()
        .args(["eval", discount_db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("165\n");
    bin()
        .args(["verify", discount_db.to_str().unwrap()])
        .assert()
        .success();

    let bool_db = temp.path().join("booleans.sqlite");
    run(&["init", bool_db.to_str().unwrap()]);
    run(&["import", bool_db.to_str().unwrap(), "examples/booleans.cdb"]);
    bin()
        .args(["eval", bool_db.to_str().unwrap(), "main"])
        .assert()
        .success()
        .stdout("5\n");
    bin()
        .args(["verify", bool_db.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn stale_expected_root_returns_conflict_without_writes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("conflict.sqlite");

    run(&["init", db.to_str().unwrap()]);
    let import = run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let expected_root = parse_line_value(&import, "root ");

    run(&["create-alias", db.to_str().unwrap(), "tax", "sales_tax"]);
    let branch_before_conflict = branch_state(&db);
    let counts_before_conflict = mutation_guard_counts(&db);

    let conflict = run(&[
        "rename",
        db.to_str().unwrap(),
        "tax",
        "vat",
        "--expect-root",
        expected_root,
    ]);
    assert!(conflict.contains("conflict rename_symbol main.tax -> main.vat"));
    assert!(conflict.contains(&format!("expected_root {expected_root}")));
    assert!(conflict.contains("failed_preconditions root_is_current"));
    assert_eq!(branch_state(&db), branch_before_conflict);
    assert_eq!(mutation_guard_counts(&db), counts_before_conflict);

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn structural_operations_retry_with_expected_root_return_already_applied() {
    let temp = tempdir().unwrap();

    let import_db = temp.path().join("import.sqlite");
    run(&["init", import_db.to_str().unwrap()]);
    run(&["import", import_db.to_str().unwrap(), "examples/shop.cdb"]);
    let branch_before_import_retry = branch_state(&import_db);
    let counts_before_import_retry = mutation_guard_counts(&import_db);
    let import_retry = run(&["import", import_db.to_str().unwrap(), "examples/shop.cdb"]);
    assert!(import_retry.contains("already_applied create_function main.tax"));
    assert!(import_retry.contains("already_applied create_function main.total"));
    assert!(import_retry.contains("already_applied create_function main.main"));
    assert_eq!(branch_state(&import_db), branch_before_import_retry);
    assert_eq!(
        mutation_guard_counts(&import_db),
        counts_before_import_retry
    );

    let replace_db = temp.path().join("replace-retry.sqlite");
    run(&["init", replace_db.to_str().unwrap()]);
    run(&["import", replace_db.to_str().unwrap(), "examples/shop.cdb"]);
    let replace = run(&[
        "replace-body",
        replace_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    let replace_expected_root = parse_line_value(&replace, "old_root ");
    assert!(replace.contains("build_impact recompile_symbols"));
    let branch_before_replace_retry = branch_state(&replace_db);
    let counts_before_replace_retry = mutation_guard_counts(&replace_db);
    let replace_retry = run(&[
        "replace-body",
        replace_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
        "--expect-root",
        replace_expected_root,
    ]);
    assert!(replace_retry.contains("already_applied replace_function_body main.tax"));
    assert_eq!(branch_state(&replace_db), branch_before_replace_retry);
    assert_eq!(
        mutation_guard_counts(&replace_db),
        counts_before_replace_retry
    );
    let branch_before_current_replace_retry = branch_state(&replace_db);
    let counts_before_current_replace_retry = mutation_guard_counts(&replace_db);
    let current_replace_retry = run(&[
        "replace-body",
        replace_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    assert!(current_replace_retry.contains("already_applied replace_function_body main.tax"));
    assert_eq!(
        branch_state(&replace_db),
        branch_before_current_replace_retry
    );
    assert_eq!(
        mutation_guard_counts(&replace_db),
        counts_before_current_replace_retry
    );

    let signature_db = temp.path().join("signature-retry.sqlite");
    let signature_source = temp.path().join("signature.cdb");
    std::fs::write(
        &signature_source,
        "fn ignore(x: i64) -> i64 = 1\n\nfn main() -> i64 = ignore(5)\n",
    )
    .unwrap();
    run(&["init", signature_db.to_str().unwrap()]);
    run(&[
        "import",
        signature_db.to_str().unwrap(),
        signature_source.to_str().unwrap(),
    ]);
    let signature = run(&[
        "change-signature",
        signature_db.to_str().unwrap(),
        "ignore",
        "(y: i64) -> i64",
    ]);
    let signature_expected_root = parse_line_value(&signature, "old_root ");
    assert!(signature.contains("build_impact metadata_only"));
    let branch_before_signature_retry = branch_state(&signature_db);
    let counts_before_signature_retry = mutation_guard_counts(&signature_db);
    let signature_retry = run(&[
        "change-signature",
        signature_db.to_str().unwrap(),
        "ignore",
        "(y: i64) -> i64",
        "--expect-root",
        signature_expected_root,
    ]);
    assert!(signature_retry.contains("already_applied change_function_signature main.ignore"));
    assert_eq!(branch_state(&signature_db), branch_before_signature_retry);
    assert_eq!(
        mutation_guard_counts(&signature_db),
        counts_before_signature_retry
    );

    let delete_db = temp.path().join("delete-retry.sqlite");
    let delete_source = temp.path().join("delete.cdb");
    std::fs::write(
        &delete_source,
        "fn unused() -> i64 = 1\n\nfn main() -> i64 = 2\n",
    )
    .unwrap();
    run(&["init", delete_db.to_str().unwrap()]);
    run(&[
        "import",
        delete_db.to_str().unwrap(),
        delete_source.to_str().unwrap(),
    ]);
    let delete = run(&["delete-symbol", delete_db.to_str().unwrap(), "unused"]);
    let delete_expected_root = parse_line_value(&delete, "old_root ");
    assert!(delete.contains("build_impact relink_only"));
    let branch_before_delete_retry = branch_state(&delete_db);
    let counts_before_delete_retry = mutation_guard_counts(&delete_db);
    let delete_retry = run(&[
        "delete-symbol",
        delete_db.to_str().unwrap(),
        "unused",
        "--expect-root",
        delete_expected_root,
    ]);
    assert!(delete_retry.contains("already_applied delete_symbol main.unused"));
    assert_eq!(branch_state(&delete_db), branch_before_delete_retry);
    assert_eq!(
        mutation_guard_counts(&delete_db),
        counts_before_delete_retry
    );

    let alias_db = temp.path().join("alias-retry.sqlite");
    run(&["init", alias_db.to_str().unwrap()]);
    run(&["import", alias_db.to_str().unwrap(), "examples/shop.cdb"]);
    let alias = run(&[
        "create-alias",
        alias_db.to_str().unwrap(),
        "tax",
        "sales_tax",
    ]);
    let alias_expected_root = parse_line_value(&alias, "old_root ");
    assert!(alias.contains("build_impact metadata_only"));
    let branch_before_alias_retry = branch_state(&alias_db);
    let counts_before_alias_retry = mutation_guard_counts(&alias_db);
    let alias_retry = run(&[
        "create-alias",
        alias_db.to_str().unwrap(),
        "tax",
        "sales_tax",
        "--expect-root",
        alias_expected_root,
    ]);
    assert!(alias_retry.contains("already_applied create_alias main.tax as main.sales_tax"));
    assert_eq!(branch_state(&alias_db), branch_before_alias_retry);
    assert_eq!(mutation_guard_counts(&alias_db), counts_before_alias_retry);
}

#[test]
fn stale_expected_root_conflicts_across_structural_operations() {
    let temp = tempdir().unwrap();

    let replace_db = temp.path().join("replace-conflict.sqlite");
    run(&["init", replace_db.to_str().unwrap()]);
    let replace_import = run(&["import", replace_db.to_str().unwrap(), "examples/shop.cdb"]);
    let replace_expected_root = parse_line_value(&replace_import, "root ");
    run(&[
        "create-alias",
        replace_db.to_str().unwrap(),
        "tax",
        "sales_tax",
    ]);
    let branch_before_replace_conflict = branch_state(&replace_db);
    let counts_before_replace_conflict = mutation_guard_counts(&replace_db);
    let replace_conflict = run(&[
        "replace-body",
        replace_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
        "--expect-root",
        replace_expected_root,
    ]);
    assert!(replace_conflict.contains("conflict replace_function_body main.tax"));
    assert_eq!(branch_state(&replace_db), branch_before_replace_conflict);
    assert_eq!(
        mutation_guard_counts(&replace_db),
        counts_before_replace_conflict
    );

    let signature_db = temp.path().join("signature-conflict.sqlite");
    let signature_source = temp.path().join("signature-conflict.cdb");
    std::fs::write(
        &signature_source,
        "fn ignore(x: i64) -> i64 = 1\n\nfn main() -> i64 = ignore(5)\n",
    )
    .unwrap();
    run(&["init", signature_db.to_str().unwrap()]);
    let signature_import = run(&[
        "import",
        signature_db.to_str().unwrap(),
        signature_source.to_str().unwrap(),
    ]);
    let signature_expected_root = parse_line_value(&signature_import, "root ");
    run(&[
        "create-alias",
        signature_db.to_str().unwrap(),
        "ignore",
        "ignored",
    ]);
    let branch_before_signature_conflict = branch_state(&signature_db);
    let counts_before_signature_conflict = mutation_guard_counts(&signature_db);
    let signature_conflict = run(&[
        "change-signature",
        signature_db.to_str().unwrap(),
        "ignore",
        "(y: i64) -> i64",
        "--expect-root",
        signature_expected_root,
    ]);
    assert!(signature_conflict.contains("conflict change_function_signature main.ignore"));
    assert_eq!(
        branch_state(&signature_db),
        branch_before_signature_conflict
    );
    assert_eq!(
        mutation_guard_counts(&signature_db),
        counts_before_signature_conflict
    );

    let delete_db = temp.path().join("delete-conflict.sqlite");
    let delete_source = temp.path().join("delete-conflict.cdb");
    std::fs::write(
        &delete_source,
        "fn unused() -> i64 = 1\n\nfn main() -> i64 = 2\n",
    )
    .unwrap();
    run(&["init", delete_db.to_str().unwrap()]);
    let delete_import = run(&[
        "import",
        delete_db.to_str().unwrap(),
        delete_source.to_str().unwrap(),
    ]);
    let delete_expected_root = parse_line_value(&delete_import, "root ");
    run(&[
        "create-alias",
        delete_db.to_str().unwrap(),
        "unused",
        "still_here",
    ]);
    let branch_before_delete_conflict = branch_state(&delete_db);
    let counts_before_delete_conflict = mutation_guard_counts(&delete_db);
    let delete_conflict = run(&[
        "delete-symbol",
        delete_db.to_str().unwrap(),
        "unused",
        "--expect-root",
        delete_expected_root,
    ]);
    assert!(delete_conflict.contains("conflict delete_symbol main.unused"));
    assert_eq!(branch_state(&delete_db), branch_before_delete_conflict);
    assert_eq!(
        mutation_guard_counts(&delete_db),
        counts_before_delete_conflict
    );

    let alias_db = temp.path().join("alias-conflict.sqlite");
    run(&["init", alias_db.to_str().unwrap()]);
    let alias_import = run(&["import", alias_db.to_str().unwrap(), "examples/shop.cdb"]);
    let alias_expected_root = parse_line_value(&alias_import, "root ");
    run(&[
        "replace-body",
        alias_db.to_str().unwrap(),
        "tax",
        "subtotal * 18 / 100",
    ]);
    let branch_before_alias_conflict = branch_state(&alias_db);
    let counts_before_alias_conflict = mutation_guard_counts(&alias_db);
    let alias_conflict = run(&[
        "create-alias",
        alias_db.to_str().unwrap(),
        "tax",
        "sales_tax",
        "--expect-root",
        alias_expected_root,
    ]);
    assert!(alias_conflict.contains("conflict create_alias main.tax as main.sales_tax"));
    assert_eq!(branch_state(&alias_db), branch_before_alias_conflict);
    assert_eq!(
        mutation_guard_counts(&alias_db),
        counts_before_alias_conflict
    );
}

#[test]
fn failed_applied_migration_rolls_back_partial_writes() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("rollback.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    let branch_before_failure = branch_state(&db);
    let counts_before_failure = mutation_guard_counts(&db);

    let stderr = run_failure(&["replace-body", db.to_str().unwrap(), "tax", "true"]);
    assert!(stderr.contains("replacement body type bool does not match return type i64"));
    assert_eq!(branch_state(&db), branch_before_failure);
    assert_eq!(mutation_guard_counts(&db), counts_before_failure);

    bin()
        .args(["verify", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn verify_rejects_cache_key_payload_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("cache-mismatch.sqlite");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, cache_key_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, cache_key_json FROM compile_cache
             WHERE artifact_kind = 'interface_hash'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&cache_key_json).unwrap();
    value["target_triple"] = JsonValue::String("aarch64-apple-darwin".to_string());
    let tampered = serde_json::to_string(&value).unwrap();
    conn.execute(
        "UPDATE compile_cache SET cache_key_json = ?1 WHERE cache_key = ?2",
        (&tampered, &cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_cache_entry"));
    assert!(stderr.contains("cache key mismatch"));
}

#[test]
fn verify_rejects_native_object_metadata_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("object-metadata-mismatch.sqlite");
    let tax_obj = temp.path().join("tax.o");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    run(&[
        "emit-object",
        db.to_str().unwrap(),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        tax_obj.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'object_file'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["target_triple"] = JsonValue::String("bad-target".to_string());
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (serde_json::to_string(&value).unwrap(), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_object_artifact"));
    assert!(stderr.contains("target"));
}

#[test]
fn verify_rejects_cached_link_plan_metadata_mismatch() {
    let temp = tempdir().unwrap();
    let db = temp.path().join("link-plan-metadata-mismatch.sqlite");
    let plan_path = temp.path().join("main.link.json");

    run(&["init", db.to_str().unwrap()]);
    run(&["import", db.to_str().unwrap(), "examples/shop.cdb"]);
    run(&[
        "link-native",
        db.to_str().unwrap(),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        plan_path.to_str().unwrap(),
    ]);

    let conn = Connection::open(&db).unwrap();
    let (cache_key, artifact_json): (String, String) = conn
        .query_row(
            "SELECT cache_key, artifact_json FROM compile_cache
             WHERE artifact_kind = 'link_plan'
             ORDER BY cache_key LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut value: JsonValue = serde_json::from_str(&artifact_json).unwrap();
    value["metadata"]["external_symbols"] = serde_json::json!(["puts"]);
    conn.execute(
        "UPDATE compile_cache SET artifact_json = ?1 WHERE cache_key = ?2",
        (serde_json::to_string(&value).unwrap(), cache_key),
    )
    .unwrap();

    let stderr = run_failure(&["verify", db.to_str().unwrap()]);
    assert!(stderr.contains("bad_link_plan"));
    assert!(stderr.contains("external symbols"));
}

fn cache_rows(db: &Path) -> Vec<(String, String, String)> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT backend, target, artifact_kind FROM compile_cache ORDER BY artifact_kind")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn cache_json_values_by_kind(db: &Path, artifact_kind: &str) -> Vec<JsonValue> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT artifact_json FROM compile_cache
             WHERE artifact_kind = ?1 ORDER BY cache_key",
        )
        .unwrap();
    stmt.query_map([artifact_kind], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
        .collect()
}

fn cache_key_json_values_by_kind(db: &Path, artifact_kind: &str) -> Vec<JsonValue> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT cache_key_json FROM compile_cache
             WHERE artifact_kind = ?1 ORDER BY cache_key",
        )
        .unwrap();
    stmt.query_map([artifact_kind], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
        .collect()
}

fn row_count(db: &Path, table: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn cache_row_count_by_kind(db: &Path, artifact_kind: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM compile_cache WHERE artifact_kind = ?1",
        [artifact_kind],
        |row| row.get(0),
    )
    .unwrap()
}

fn object_cache_entry_by_definition(db: &Path, definition: &str) -> (JsonValue, Vec<u8>) {
    let conn = Connection::open(db).unwrap();
    let (artifact_json, artifact_bytes): (String, Vec<u8>) = conn
        .query_row(
            "SELECT artifact_json, artifact_bytes FROM compile_cache
             WHERE artifact_kind = 'object_file' AND input_hash = ?1
             ORDER BY cache_key LIMIT 1",
            [definition],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    (
        serde_json::from_str(&artifact_json).unwrap(),
        artifact_bytes,
    )
}

fn branch_state(db: &Path) -> (String, Option<String>) {
    let conn = Connection::open(db).unwrap();
    conn.query_row(
        "SELECT root_hash, history_hash FROM branches WHERE name = 'main'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn mutation_guard_counts(db: &Path) -> Vec<(String, i64)> {
    [
        "objects",
        "migrations",
        "histories",
        "root_symbols",
        "root_names",
        "root_exports",
        "dependencies",
        "compile_cache",
        "source_search",
    ]
    .into_iter()
    .map(|table| (table.to_string(), row_count(db, table)))
    .collect()
}

fn compile_and_run_c_if_available(c_file: &Path) {
    if StdCommand::new("cc").arg("--version").output().is_err() {
        return;
    }
    let dir = c_file.parent().unwrap();
    let harness = dir.join("harness.c");
    let exe = dir.join("harness");
    std::fs::write(
        &harness,
        "long codedb_main(void);\nint main(void) { return codedb_main() == 120 ? 0 : 1; }\n",
    )
    .unwrap();
    let status = StdCommand::new("cc")
        .arg(c_file)
        .arg(&harness)
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("run cc");
    assert!(status.success());
    let status = StdCommand::new(&exe).status().expect("run c harness");
    assert!(status.success());
}

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn link_and_run_native_if_linux(dir: &Path, objects: &[&Path], entry_symbol: &str, expected: i64) {
    if std::env::consts::OS != "linux" {
        return;
    }
    link_and_run_native(dir, objects, entry_symbol, expected);
}

fn link_and_run_native_if_apple_silicon(
    dir: &Path,
    objects: &[&Path],
    entry_symbol: &str,
    expected: i64,
) {
    if !is_apple_silicon() {
        return;
    }
    link_and_run_native(dir, objects, entry_symbol, expected);
}

fn link_and_run_native(dir: &Path, objects: &[&Path], entry_symbol: &str, expected: i64) {
    if StdCommand::new("cc").arg("--version").output().is_err() {
        return;
    }
    let harness = dir.join("native_harness.c");
    let exe = dir.join("native_harness");
    std::fs::write(
        &harness,
        format!(
            "long {entry_symbol}(void);\nint main(void) {{ return {entry_symbol}() == {expected} ? 0 : 1; }}\n"
        ),
    )
    .unwrap();
    let mut command = StdCommand::new("cc");
    for object in objects {
        command.arg(object);
    }
    let status = command
        .arg(&harness)
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("link native harness");
    assert!(status.success());
    let status = StdCommand::new(&exe).status().expect("run native harness");
    assert!(status.success());
}

fn is_apple_silicon() -> bool {
    std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64"
}
