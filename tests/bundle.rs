use std::path::Path;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
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

fn cache_kinds_in_bundle(bundle: &Path) -> Vec<String> {
    let bundle_json = parse_json(&std::fs::read_to_string(bundle).unwrap());
    let mut kinds = bundle_json["artifact_cache"]
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| {
            artifact["cache_key_input"]["artifact_kind"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    kinds.sort();
    kinds
}

fn test_bytes_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/bytes/v1\0");
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
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
fn bundle_export_import_preserves_v2_type_closure() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source-v2.sqlite");
    let imported_db = temp.path().join("imported-v2.sqlite");
    let bundle = temp.path().join("line-view-refs.codedb.bundle");

    run(&["init", path(&source_db)]);
    run(&["import", path(&source_db), "examples/v2/line_view_refs.cdb"]);
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

    run(&["init", path(&imported_db)]);
    let import_report = run(&["bundle", "import", path(&imported_db), path(&bundle)]);
    assert!(import_report.contains("imported bundle"));
    assert_eq!(branch_state(&imported_db), source_branch);
    assert_eq!(run(&["eval", path(&imported_db), "main"]), "100\n");
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
    run(&[
        "link-native",
        path(&source_db),
        "main",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&temp.path().join("link-plan.json")),
    ]);
    assert_eq!(cache_row_count_by_kind(&source_db, "link_plan"), 1);
    assert!(cache_row_count_by_kind(&source_db, "interface_hash") > 0);
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
    assert!(!bundle_json["artifact_cache"].as_array().unwrap().is_empty());
    let bundle_cache_kinds = cache_kinds_in_bundle(&bundle);
    assert!(bundle_cache_kinds.contains(&"interface_hash".to_string()));
    assert!(bundle_cache_kinds.contains(&"link_plan".to_string()));

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

    let imported_with_artifacts = temp.path().join("imported-with-artifacts.sqlite");
    run(&["init", path(&imported_with_artifacts)]);
    run(&[
        "bundle",
        "import",
        path(&imported_with_artifacts),
        path(&bundle),
        "--import-artifacts",
    ]);
    assert_eq!(branch_state(&imported_with_artifacts), source_branch);
    assert_eq!(
        cache_row_count_by_kind(&imported_with_artifacts, "link_plan"),
        1
    );
    assert!(cache_row_count_by_kind(&imported_with_artifacts, "interface_hash") > 0);
    bin()
        .args(["verify", path(&imported_with_artifacts)])
        .assert()
        .success()
        .stdout("verify ok\n");
}

#[test]
fn bundle_artifact_cache_includes_type_layout_inputs_outside_root_closure() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source-layout-artifact.sqlite");
    let imported_db = temp.path().join("imported-layout-artifact.sqlite");
    let source = temp.path().join("layout-artifact.cdb");
    let layout_path = temp.path().join("unused-layout.json");
    let bundle = temp.path().join("layout-artifact.codedb.bundle");

    std::fs::write(
        &source,
        r#"
record Unused {
  value: i64
}

fn main() -> i64 = 7
"#,
    )
    .unwrap();

    run(&["init", path(&source_db)]);
    run(&["import", path(&source_db), path(&source)]);
    run(&[
        "emit-type-layout",
        path(&source_db),
        "Unused",
        "--out",
        path(&layout_path),
    ]);
    assert_eq!(cache_row_count_by_kind(&source_db, "type_layout"), 1);
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
    assert!(cache_kinds_in_bundle(&bundle).contains(&"type_layout".to_string()));

    run(&["init", path(&imported_db)]);
    run(&[
        "bundle",
        "import",
        path(&imported_db),
        path(&bundle),
        "--import-artifacts",
    ]);
    assert_eq!(branch_state(&imported_db), source_branch);
    assert_eq!(cache_row_count_by_kind(&imported_db, "type_layout"), 1);
    bin()
        .args(["verify", path(&imported_db)])
        .assert()
        .success()
        .stdout("verify ok\n");
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

#[test]
fn bundle_import_rejects_tampered_migration_agent_metadata() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source-agent-tamper.sqlite");
    let tampered_db = temp.path().join("tampered-agent.sqlite");
    let bundle = temp.path().join("shop.codedb.bundle");
    let tampered = temp.path().join("shop-agent-tampered.codedb.bundle");

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
    bundle_json["migrations"][0]["agent"] = json!({ "agent_id": "tampered" });
    std::fs::write(
        &tampered,
        format!("{}\n", serde_json::to_string(&bundle_json).unwrap()),
    )
    .unwrap();

    run(&["init", path(&tampered_db)]);
    let stderr = run_failure(&["bundle", "import", path(&tampered_db), path(&tampered)]);
    assert!(stderr.contains("bad_bundle_history"), "{stderr}");
    assert!(stderr.contains("audit hash"), "{stderr}");
}

#[test]
fn bundle_import_rejects_extra_unreachable_object() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source-extra.sqlite");
    let unrelated_db = temp.path().join("unrelated-extra.sqlite");
    let tampered_db = temp.path().join("tampered-extra.sqlite");
    let source = temp.path().join("source.cdb");
    let unrelated_source = temp.path().join("unrelated.cdb");
    let bundle = temp.path().join("source.codedb.bundle");
    let unrelated_bundle = temp.path().join("unrelated.codedb.bundle");
    let tampered = temp.path().join("source-extra-object.codedb.bundle");

    std::fs::write(&source, "fn main() -> i64 = 1\n").unwrap();
    std::fs::write(&unrelated_source, "fn other() -> i64 = 2\n").unwrap();

    run(&["init", path(&source_db)]);
    run(&["import", path(&source_db), path(&source)]);
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

    run(&["init", path(&unrelated_db)]);
    run(&["import", path(&unrelated_db), path(&unrelated_source)]);
    let unrelated_branch = branch_state(&unrelated_db);
    run(&[
        "bundle",
        "export",
        path(&unrelated_db),
        "--root",
        &unrelated_branch.0,
        "--out",
        path(&unrelated_bundle),
    ]);

    let mut bundle_json = parse_json(&std::fs::read_to_string(&bundle).unwrap());
    let unrelated_json = parse_json(&std::fs::read_to_string(&unrelated_bundle).unwrap());
    let source_hashes = bundle_json["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|object| object["hash"].as_str().unwrap().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let extra = unrelated_json["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|object| !source_hashes.contains(object["hash"].as_str().unwrap()))
        .expect("unrelated object")
        .clone();
    let objects = bundle_json["objects"].as_array_mut().unwrap();
    objects.push(extra);
    bundle_json["manifest"]["object_count"] = json!(objects.len());
    std::fs::write(
        &tampered,
        format!("{}\n", serde_json::to_string(&bundle_json).unwrap()),
    )
    .unwrap();

    run(&["init", path(&tampered_db)]);
    let stderr = run_failure(&["bundle", "import", path(&tampered_db), path(&tampered)]);
    assert!(stderr.contains("bad_bundle_closure"), "{stderr}");
}

#[test]
fn bundle_import_rejects_tampered_json_artifact_cache() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source-artifact-tamper.sqlite");
    let tampered_db = temp.path().join("tampered-artifacts.sqlite");
    let bundle = temp.path().join("with-artifacts.codedb.bundle");
    let tampered = temp.path().join("with-artifacts-tampered.codedb.bundle");
    let ir = temp.path().join("main.ir.json");

    run(&["init", path(&source_db)]);
    run(&["import", path(&source_db), "examples/shop.cdb"]);
    run(&["emit-ir", path(&source_db), "main", "--out", path(&ir)]);
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

    let mut bundle_json = parse_json(&std::fs::read_to_string(&bundle).unwrap());
    let artifact = bundle_json["artifact_cache"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|artifact| {
            artifact["artifact_json"].is_object() && artifact["artifact_bytes_hex"].is_null()
        })
        .expect("JSON-only artifact cache entry");
    artifact["artifact_json"]["metadata"]["schema"] = json!("tampered/artifact/v1");
    std::fs::write(
        &tampered,
        format!("{}\n", serde_json::to_string(&bundle_json).unwrap()),
    )
    .unwrap();

    run(&["init", path(&tampered_db)]);
    let stderr = run_failure(&[
        "bundle",
        "import",
        path(&tampered_db),
        path(&tampered),
        "--import-artifacts",
    ]);
    assert!(stderr.contains("bad_bundle_artifact"), "{stderr}");
    assert!(stderr.contains("recomputes to"), "{stderr}");
}

#[test]
fn bundle_import_rejects_tampered_object_artifact_bytes() {
    let temp = tempdir().unwrap();
    let source_db = temp.path().join("source-object-tamper.sqlite");
    let tampered_db = temp.path().join("tampered-object-artifacts.sqlite");
    let bundle = temp.path().join("with-object-artifact.codedb.bundle");
    let tampered = temp
        .path()
        .join("with-object-artifact-tampered.codedb.bundle");
    let object = temp.path().join("tax.o");

    run(&["init", path(&source_db)]);
    run(&["import", path(&source_db), "examples/shop.cdb"]);
    run(&[
        "emit-object",
        path(&source_db),
        "tax",
        "--target",
        codedb::LINUX_X86_64_TARGET,
        "--out",
        path(&object),
    ]);
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

    let mut bundle_json = parse_json(&std::fs::read_to_string(&bundle).unwrap());
    let artifact = bundle_json["artifact_cache"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|artifact| artifact["cache_key_input"]["artifact_kind"] == "object_file")
        .expect("object artifact cache entry");
    let bad_bytes = b"not a native object\n";
    let bad_hash = test_bytes_hash(bad_bytes);
    artifact["artifact_hash"] = json!(bad_hash);
    artifact["artifact_bytes_hex"] = json!(hex::encode(bad_bytes));
    artifact["artifact_json"]["bytes_hash"] = artifact["artifact_hash"].clone();
    std::fs::write(
        &tampered,
        format!("{}\n", serde_json::to_string(&bundle_json).unwrap()),
    )
    .unwrap();

    run(&["init", path(&tampered_db)]);
    let stderr = run_failure(&[
        "bundle",
        "import",
        path(&tampered_db),
        path(&tampered),
        "--import-artifacts",
    ]);
    assert!(stderr.contains("bad_bundle_artifact"), "{stderr}");
    assert!(stderr.contains("bad_object_artifact"), "{stderr}");
    assert_eq!(cache_row_count_by_kind(&tampered_db, "object_file"), 0);
}
