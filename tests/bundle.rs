use std::path::Path;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
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

fn branch_state(db: &Path) -> (String, Option<String>) {
    let branches = parse_json(&run(&["branches", path(db), "--json"]));
    let branch = branches["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|branch| branch["name"] == "main")
        .expect("main branch");
    (
        branch["root_hash"].as_str().unwrap().to_string(),
        branch["history_hash"].as_str().map(str::to_string),
    )
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

#[test]
fn bundle_export_import_reconstructs_object_closure_and_verifies() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source.sqlite");
    let imported_db = temp.path().join("imported.sqlite");
    let bundle = temp.path().join("shop.codedb.bundle");

    run(&["init", path(&source_db)]);
    run(&["import", path(&source_db), "examples/shop.cdb"]);
    let source_branch = branch_state(&source_db);

    let export_report = run(&[
        "bundle",
        "export",
        path(&source_db),
        "--root",
        &source_branch.0,
        "--out",
        path(&bundle),
    ]);
    assert!(export_report.contains("exported bundle"));

    let bundle_json = parse_json(&std::fs::read_to_string(&bundle).unwrap());
    assert_eq!(bundle_json["schema"], "codedb/bundle/v1");
    assert_eq!(bundle_json["manifest"]["root_hash"], source_branch.0);
    assert_eq!(
        bundle_json["manifest"]["history_hash"],
        serde_json::to_value(&source_branch.1).unwrap()
    );
    assert_eq!(bundle_json["manifest"]["migration_count"], 3);
    assert!(bundle_json["objects"].as_array().unwrap().len() > 10);
    assert_eq!(
        bundle_json["manifest"]["requires_projection_sources"],
        false
    );

    run(&["init", path(&imported_db)]);
    let import_report = run(&["bundle", "import", path(&imported_db), path(&bundle)]);
    assert!(import_report.contains("imported bundle"));
    assert_eq!(branch_state(&imported_db), source_branch);
    assert_eq!(run(&["eval", path(&imported_db), "main"]), "120\n");
    bin()
        .args(["verify", path(&imported_db)])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn bundle_artifact_cache_is_optional_and_can_be_regenerated() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source-artifacts.sqlite");
    let imported_db = temp.path().join("imported-artifacts.sqlite");
    let bundle = temp.path().join("with-artifacts.codedb.bundle");
    let source_object = temp.path().join("source-tax.o");
    let imported_object = temp.path().join("imported-tax.o");

    run(&["init", path(&source_db)]);
    run(&["import", path(&source_db), "examples/shop.cdb"]);
    run(&[
        "emit-object",
        path(&source_db),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&source_object),
    ]);
    assert_eq!(cache_row_count_by_kind(&source_db, "object_file"), 1);
    let source_branch = branch_state(&source_db);

    run(&[
        "bundle",
        "export",
        path(&source_db),
        "--root",
        &source_branch.0,
        "--out",
        path(&bundle),
        "--include-artifacts",
    ]);
    let bundle_json = parse_json(&std::fs::read_to_string(&bundle).unwrap());
    assert_eq!(bundle_json["manifest"]["artifact_cache_included"], true);
    assert!(bundle_json["artifact_cache"].as_array().unwrap().len() >= 1);

    run(&["init", path(&imported_db)]);
    assert_eq!(cache_row_count_by_kind(&imported_db, "object_file"), 0);
    run(&["bundle", "import", path(&imported_db), path(&bundle)]);
    assert_eq!(branch_state(&imported_db), source_branch);
    assert_eq!(cache_row_count_by_kind(&imported_db, "object_file"), 0);

    run(&[
        "emit-object",
        path(&imported_db),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&imported_object),
    ]);
    let bytes = std::fs::read(&imported_object).unwrap();
    assert_eq!(&bytes[..4], b"\x7fELF");
    assert_eq!(cache_row_count_by_kind(&imported_db, "object_file"), 1);
}

#[test]
fn bundle_import_rejects_tampered_bundle() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source-tamper.sqlite");
    let tampered_db = temp.path().join("tampered.sqlite");
    let bundle = temp.path().join("shop.codedb.bundle");
    let tampered = temp.path().join("shop-tampered.codedb.bundle");

    run(&["init", path(&source_db)]);
    run(&["import", path(&source_db), "examples/shop.cdb"]);
    let source_branch = branch_state(&source_db);
    run(&[
        "bundle",
        "export",
        path(&source_db),
        "--root",
        &source_branch.0,
        "--out",
        path(&bundle),
    ]);

    let mut bundle_json = parse_json(&std::fs::read_to_string(&bundle).unwrap());
    bundle_json["manifest"]["root_hash"] = json!("sha256:tampered");
    std::fs::write(
        &tampered,
        format!("{}\n", serde_json::to_string(&bundle_json).unwrap()),
    )
    .unwrap();

    run(&["init", path(&tampered_db)]);
    let stderr = run_failure(&["bundle", "import", path(&tampered_db), path(&tampered)]);
    assert!(stderr.contains("bad_bundle"));
}
