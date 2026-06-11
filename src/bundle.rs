use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::artifact::{ARTIFACT_METADATA_SCHEMA, CacheKeyInput};
use crate::migrations::{Operation, history_hash, migration_hash};
use crate::model::ProgramRootPayload;
use crate::store::{
    CodeDb, cache_key_for_input, canonical_json, extract_hash_strings, hash_bytes,
    hash_object_canonical,
};
use crate::{BYTES_DOMAIN, MAIN_BRANCH};

const BUNDLE_SCHEMA: &str = "codedb/bundle/v1";
const BUNDLE_MANIFEST_SCHEMA: &str = "codedb/bundle-manifest/v1";
const BUNDLE_OBJECT_SCHEMA: &str = "codedb/bundle-object/v1";
const BUNDLE_MIGRATION_SCHEMA: &str = "codedb/bundle-migration/v1";
const BUNDLE_MIGRATION_AUDIT_SCHEMA: &str = "codedb/bundle-migration-audit/v1";
const BUNDLE_ARTIFACT_SCHEMA: &str = "codedb/bundle-artifact/v1";
const BUNDLE_API_SCHEMA: &str = "codedb/bundle-api/v1";
const PACKAGE_IDENTITY_SCHEMA: &str = "codedb/package-identity/v1";

#[derive(Debug, Clone)]
pub struct BundleExport {
    pub text: String,
    pub root_hash: String,
    pub history_hash: Option<String>,
    pub package_hash: String,
    pub object_count: usize,
    pub migration_count: usize,
    pub artifact_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleDocument {
    schema: String,
    manifest: BundleManifest,
    objects: Vec<BundleObject>,
    migrations: Vec<BundleMigration>,
    #[serde(default)]
    artifact_cache: Vec<BundleArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    schema: String,
    root_hash: String,
    history_hash: Option<String>,
    api_hash: String,
    package_hash: String,
    object_count: usize,
    migration_count: usize,
    artifact_count: usize,
    artifact_cache_included: bool,
    requires_projection_sources: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleObject {
    schema: String,
    hash: String,
    kind: String,
    schema_version: i64,
    payload: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleMigration {
    schema: String,
    sequence: usize,
    migration_hash: String,
    history_hash: String,
    parent_history_hash: Option<String>,
    input_root_hash: String,
    output_root_hash: String,
    operation_kind: String,
    operation: JsonValue,
    preconditions: JsonValue,
    postconditions: JsonValue,
    agent: JsonValue,
    audit_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleArtifact {
    schema: String,
    cache_key: String,
    cache_key_input: JsonValue,
    artifact_hash: String,
    artifact_json: Option<JsonValue>,
    artifact_bytes_hex: Option<String>,
}

impl CodeDb {
    pub fn export_bundle_root(
        &self,
        root_hash: &str,
        include_artifacts: bool,
    ) -> Result<BundleExport> {
        self.load_root(root_hash)
            .with_context(|| format!("bundle root is not a valid program root: {root_hash}"))?;
        let history_hash = self.history_head_for_root(root_hash)?;
        let migrations = match history_hash.as_deref() {
            Some(history_hash) => self.bundle_history_slice(history_hash)?,
            None => Vec::new(),
        };

        let mut closure_roots = BTreeSet::from([root_hash.to_string()]);
        for migration in &migrations {
            closure_roots.insert(migration.input_root_hash.clone());
            closure_roots.insert(migration.output_root_hash.clone());
        }
        let root_objects = self.bundle_object_closure(&closure_roots)?;
        let root_object_hashes = root_objects
            .iter()
            .map(|object| object.hash.clone())
            .collect::<BTreeSet<_>>();
        let mut objects_by_hash = root_objects
            .into_iter()
            .map(|object| (object.hash.clone(), object))
            .collect::<BTreeMap<_, _>>();
        if include_artifacts {
            for object in self.bundle_artifact_input_objects(&root_object_hashes)? {
                objects_by_hash.entry(object.hash.clone()).or_insert(object);
            }
        }
        let object_hashes = objects_by_hash.keys().cloned().collect::<BTreeSet<_>>();
        let artifact_cache = if include_artifacts {
            self.bundle_artifact_cache(&object_hashes)?
        } else {
            Vec::new()
        };
        let objects = objects_by_hash.into_values().collect::<Vec<_>>();

        let api_hash = bundle_api_hash();
        let package_hash = package_hash(root_hash, history_hash.as_deref(), &api_hash);
        let manifest = BundleManifest {
            schema: BUNDLE_MANIFEST_SCHEMA.to_string(),
            root_hash: root_hash.to_string(),
            history_hash: history_hash.clone(),
            api_hash,
            package_hash: package_hash.clone(),
            object_count: objects.len(),
            migration_count: migrations.len(),
            artifact_count: artifact_cache.len(),
            artifact_cache_included: include_artifacts,
            requires_projection_sources: false,
        };
        let document = BundleDocument {
            schema: BUNDLE_SCHEMA.to_string(),
            manifest,
            objects,
            migrations,
            artifact_cache,
        };
        let text = format!("{}\n", canonical_json(&serde_json::to_value(&document)?));
        Ok(BundleExport {
            text,
            root_hash: root_hash.to_string(),
            history_hash,
            package_hash,
            object_count: document.manifest.object_count,
            migration_count: document.manifest.migration_count,
            artifact_count: document.manifest.artifact_count,
        })
    }

    pub fn import_bundle_file(&mut self, path: &Path, import_artifacts: bool) -> Result<String> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        self.import_bundle_str(&text, import_artifacts)
            .with_context(|| format!("failed to import {}", path.display()))
    }

    pub fn import_bundle_str(&mut self, text: &str, import_artifacts: bool) -> Result<String> {
        self.ensure_initialized()?;
        let document = parse_bundle_document(text)?;
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.import_bundle_document(document, import_artifacts);
        match result {
            Ok(report) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(report)
            }
            Err(err) => {
                if let Err(rollback_err) = self.conn.execute_batch("ROLLBACK") {
                    return Err(err).context(format!("rollback failed: {rollback_err}"));
                }
                Err(err)
            }
        }
    }

    fn import_bundle_document(
        &mut self,
        document: BundleDocument,
        import_artifacts: bool,
    ) -> Result<String> {
        validate_bundle_manifest(&document)?;
        let objects_by_hash = validate_bundle_objects(&document)?;
        validate_bundle_object_closure(&document, &objects_by_hash)?;

        for object in objects_by_hash.values() {
            self.insert_bundle_object(object)?;
        }
        for object in objects_by_hash.values() {
            self.refresh_edges(&object.hash, &object.payload)?;
        }
        for object in objects_by_hash.values() {
            if object.kind == "ProgramRoot" {
                self.index_root(&object.hash)
                    .with_context(|| format!("failed to index bundle root {}", object.hash))?;
            }
        }

        let genesis_root = self.put_program_root(&ProgramRootPayload {
            symbols: vec![],
            types: vec![],
            names: vec![],
            type_names: vec![],
            param_names: vec![],
            exports: vec![],
            tests: vec![],
            recursion_groups: vec![],
            type_recursion_groups: vec![],
            metadata: BTreeMap::new(),
        })?;
        let mut replay_root = genesis_root.clone();
        let mut replay_history: Option<String> = None;
        for (expected_sequence, migration) in document.migrations.iter().enumerate() {
            self.validate_and_insert_bundle_migration(
                expected_sequence,
                migration,
                &mut replay_root,
                &mut replay_history,
            )?;
        }
        if replay_root != document.manifest.root_hash {
            bail!(
                "bad_bundle_history: final root mismatch, expected {}, replayed {}",
                document.manifest.root_hash,
                replay_root
            );
        }
        if replay_history != document.manifest.history_hash {
            bail!(
                "bad_bundle_history: final history mismatch, expected {:?}, replayed {:?}",
                document.manifest.history_hash,
                replay_history
            );
        }

        let mut imported_artifacts = 0usize;
        if import_artifacts {
            for artifact in &document.artifact_cache {
                self.import_bundle_artifact(artifact)?;
                imported_artifacts += 1;
            }
        }

        if let Some(history_hash) = document.manifest.history_hash.as_deref() {
            self.update_branch(MAIN_BRANCH, &document.manifest.root_hash, history_hash)?;
        } else if document.manifest.root_hash != genesis_root {
            bail!("bad_bundle_history: non-genesis bundle root requires a migration slice");
        }
        if import_artifacts && imported_artifacts > 0 {
            self.verify()
                .context("bad_bundle_artifact: imported artifact cache failed verification")?;
        }

        Ok(format!(
            "imported bundle\nroot {}\nhistory {}\npackage {}\nobjects {}\nmigrations {}\nartifacts {}\n",
            document.manifest.root_hash,
            document
                .manifest
                .history_hash
                .unwrap_or_else(|| "none".to_string()),
            document.manifest.package_hash,
            document.manifest.object_count,
            document.manifest.migration_count,
            imported_artifacts,
        ))
    }

    fn history_head_for_root(&self, root_hash: &str) -> Result<Option<String>> {
        let branch_history = self
            .conn
            .query_row(
                "SELECT history_hash
                 FROM branches
                 WHERE root_hash = ?1 AND history_hash IS NOT NULL
                 ORDER BY CASE WHEN name = ?2 THEN 0 ELSE 1 END, name
                 LIMIT 1",
                params![root_hash, MAIN_BRANCH],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if branch_history.is_some() {
            return Ok(branch_history);
        }
        self.conn
            .query_row(
                "SELECT history_hash
                 FROM histories
                 WHERE output_root_hash = ?1
                 ORDER BY created_at, history_hash
                 LIMIT 1",
                params![root_hash],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    fn bundle_history_slice(&self, history_head: &str) -> Result<Vec<BundleMigration>> {
        let mut cursor = Some(history_head.to_string());
        let mut rows = Vec::new();
        let mut seen = BTreeSet::new();
        while let Some(history_hash_value) = cursor {
            if !seen.insert(history_hash_value.clone()) {
                bail!("bad_history_link: history chain contains a cycle at {history_hash_value}");
            }
            let (parent_history_hash, migration_hash_value, history_output_root): (
                Option<String>,
                String,
                String,
            ) = self.conn.query_row(
                "SELECT parent_history_hash, migration_hash, output_root_hash
                 FROM histories WHERE history_hash = ?1",
                params![&history_hash_value],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let (
                migration_parent_history,
                input_root_hash,
                output_root_hash,
                operation_kind,
                operation_json,
                preconditions_json,
                postconditions_json,
                agent_json,
            ): (
                Option<String>,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
            ) = self.conn.query_row(
                "SELECT parent_history_hash, input_root_hash, output_root_hash,
                        operation_kind, operation_json, preconditions_json,
                        postconditions_json, agent_json
                 FROM migrations WHERE hash = ?1",
                params![&migration_hash_value],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )?;
            if migration_parent_history != parent_history_hash {
                bail!(
                    "bad_history_link: history {history_hash_value} parent does not match migration {migration_hash_value}"
                );
            }
            if output_root_hash != history_output_root {
                bail!(
                    "bad_history_link: history {history_hash_value} output does not match migration {migration_hash_value}"
                );
            }
            rows.push(BundleMigration {
                schema: BUNDLE_MIGRATION_SCHEMA.to_string(),
                sequence: 0,
                migration_hash: migration_hash_value,
                history_hash: history_hash_value,
                parent_history_hash: parent_history_hash.clone(),
                input_root_hash,
                output_root_hash,
                operation_kind,
                operation: serde_json::from_str(&operation_json)?,
                preconditions: serde_json::from_str(&preconditions_json)?,
                postconditions: serde_json::from_str(&postconditions_json)?,
                agent: serde_json::from_str(&agent_json)?,
                audit_hash: String::new(),
            });
            cursor = parent_history_hash;
        }
        rows.reverse();
        for (sequence, row) in rows.iter_mut().enumerate() {
            row.sequence = sequence;
            row.audit_hash = bundle_migration_audit_hash(row)?;
        }
        Ok(rows)
    }

    fn bundle_object_closure(&self, roots: &BTreeSet<String>) -> Result<Vec<BundleObject>> {
        let mut seen = BTreeSet::new();
        let mut frontier = roots.iter().cloned().collect::<Vec<_>>();
        let mut objects = BTreeMap::new();
        while let Some(hash) = frontier.pop() {
            if !seen.insert(hash.clone()) {
                continue;
            }
            let Some((kind, schema_version, payload_json)) = self.object_row(&hash)? else {
                bail!("missing object {hash}");
            };
            let payload: JsonValue = serde_json::from_str(&payload_json)
                .with_context(|| format!("object {hash} has invalid JSON"))?;
            let canonical_payload = canonical_json(&payload);
            if canonical_payload != payload_json {
                bail!("corrupt_object: payload is not canonical {hash}");
            }
            let recomputed = hash_object_canonical(&kind, schema_version, &canonical_payload);
            if recomputed != hash {
                bail!("bad_hash: object {hash} recomputes to {recomputed}");
            }
            let mut refs = Vec::new();
            extract_hash_strings(&payload, &mut refs);
            for child_hash in refs {
                if !seen.contains(&child_hash) && self.object_exists(&child_hash)? {
                    frontier.push(child_hash);
                }
            }
            objects.insert(
                hash.clone(),
                BundleObject {
                    schema: BUNDLE_OBJECT_SCHEMA.to_string(),
                    hash,
                    kind,
                    schema_version,
                    payload,
                },
            );
        }
        Ok(objects.into_values().collect())
    }

    fn bundle_artifact_cache(
        &self,
        object_hashes: &BTreeSet<String>,
    ) -> Result<Vec<BundleArtifact>> {
        let mut stmt = self.conn.prepare(
            "SELECT cache_key, cache_key_json, input_hash, artifact_hash,
                    artifact_json, artifact_bytes
             FROM compile_cache
             ORDER BY cache_key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        })?;
        let mut artifacts = Vec::new();
        for row in rows {
            let (
                cache_key,
                cache_key_json,
                input_hash,
                artifact_hash,
                artifact_json,
                artifact_bytes,
            ) = row?;
            if !object_hashes.contains(&input_hash) {
                continue;
            }
            let cache_key_input = serde_json::from_str::<JsonValue>(&cache_key_json)
                .with_context(|| format!("cache key {cache_key} has invalid JSON"))?;
            if canonical_json(&cache_key_input) != cache_key_json {
                bail!("bad_cache: cache key {cache_key} JSON is not canonical");
            }
            let artifact_json = artifact_json
                .map(|text| serde_json::from_str::<JsonValue>(&text))
                .transpose()
                .with_context(|| format!("cache artifact {cache_key} has invalid JSON"))?;
            artifacts.push(BundleArtifact {
                schema: BUNDLE_ARTIFACT_SCHEMA.to_string(),
                cache_key,
                cache_key_input,
                artifact_hash,
                artifact_json,
                artifact_bytes_hex: artifact_bytes.map(hex::encode),
            });
        }
        Ok(artifacts)
    }

    fn bundle_artifact_input_objects(
        &self,
        root_object_hashes: &BTreeSet<String>,
    ) -> Result<Vec<BundleObject>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT input_hash
             FROM compile_cache
             ORDER BY input_hash",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut objects = BTreeMap::new();
        for row in rows {
            let input_hash = row?;
            if root_object_hashes.contains(&input_hash) || !self.object_exists(&input_hash)? {
                continue;
            }
            let roots = BTreeSet::from([input_hash]);
            let closure = self.bundle_object_closure(&roots)?;
            if closure.iter().any(|object| {
                !root_object_hashes.contains(&object.hash)
                    && !artifact_input_object_kind_is_supported(object)
            }) {
                continue;
            }
            for object in closure {
                if root_object_hashes.contains(&object.hash) {
                    continue;
                }
                objects.entry(object.hash.clone()).or_insert(object);
            }
        }
        Ok(objects.into_values().collect())
    }

    fn object_row(&self, hash: &str) -> Result<Option<(String, i64, String)>> {
        self.conn
            .query_row(
                "SELECT kind, schema_version, payload_json FROM objects WHERE hash = ?1",
                params![hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    fn object_exists(&self, hash: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                params![hash],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    fn insert_bundle_object(&mut self, object: &BundleObject) -> Result<()> {
        let canonical_payload = canonical_json(&object.payload);
        match self.object_row(&object.hash)? {
            Some((kind, schema_version, payload_json)) => {
                if kind != object.kind
                    || schema_version != object.schema_version
                    || payload_json != canonical_payload
                {
                    bail!(
                        "bad_bundle_conflict: existing object {} has different content",
                        object.hash
                    );
                }
            }
            None => {
                self.conn.execute(
                    "INSERT INTO objects
                     (hash, kind, schema_version, payload_json, payload_size_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        &object.hash,
                        &object.kind,
                        object.schema_version,
                        &canonical_payload,
                        canonical_payload.len() as i64,
                    ],
                )?;
            }
        }
        Ok(())
    }

    fn validate_and_insert_bundle_migration(
        &mut self,
        expected_sequence: usize,
        row: &BundleMigration,
        current_root: &mut String,
        current_history: &mut Option<String>,
    ) -> Result<()> {
        if row.schema != BUNDLE_MIGRATION_SCHEMA {
            bail!("bad_bundle_history: unsupported migration schema");
        }
        if row.sequence != expected_sequence {
            bail!(
                "bad_bundle_history: expected migration sequence {expected_sequence}, got {}",
                row.sequence
            );
        }
        let recomputed_audit = bundle_migration_audit_hash(row)?;
        if recomputed_audit != row.audit_hash {
            bail!(
                "bad_bundle_history: migration {} audit hash recomputes to {}",
                row.migration_hash,
                recomputed_audit
            );
        }
        if row.parent_history_hash != *current_history {
            bail!(
                "bad_bundle_history: migration {} parent {:?} does not match current {:?}",
                row.migration_hash,
                row.parent_history_hash,
                current_history
            );
        }
        if row.input_root_hash != *current_root {
            bail!(
                "bad_bundle_history: migration {} expected input {}, import has {}",
                row.migration_hash,
                row.input_root_hash,
                current_root
            );
        }
        let operation: Operation = serde_json::from_value(row.operation.clone())
            .with_context(|| format!("bad_bundle_history: invalid operation {}", row.sequence))?;
        if operation.kind_name() != row.operation_kind {
            bail!(
                "bad_bundle_history: operation kind mismatch for {}: row has {}, operation has {}",
                row.migration_hash,
                row.operation_kind,
                operation.kind_name()
            );
        }
        let recomputed_migration = migration_hash(
            current_history.as_deref(),
            current_root,
            &row.output_root_hash,
            &row.operation,
            &row.preconditions,
            &row.postconditions,
        );
        if recomputed_migration != row.migration_hash {
            bail!(
                "bad_bundle_history: migration {} recomputes to {}",
                row.migration_hash,
                recomputed_migration
            );
        }

        let expected_preconditions = canonical_json(&serde_json::to_value(
            self.preconditions_for(current_root, &operation),
        )?);
        if expected_preconditions != canonical_json(&row.preconditions) {
            bail!(
                "bad_bundle_history: preconditions changed for {}",
                row.migration_hash
            );
        }
        let failed_preconditions = self.failed_preconditions(
            current_root,
            &self.preconditions_for(current_root, &operation),
        )?;
        if !failed_preconditions.is_empty() {
            bail!(
                "bad_bundle_history: migration {} failed preconditions",
                row.migration_hash
            );
        }

        let produced =
            self.apply_operation_to_root(current_root, current_history.as_deref(), &operation)?;
        if produced != row.output_root_hash {
            bail!(
                "bad_bundle_history: migration {} expected output {}, produced {}",
                row.migration_hash,
                row.output_root_hash,
                produced
            );
        }
        let expected_postconditions = canonical_json(&serde_json::to_value(
            self.postconditions_for(&produced, &operation),
        )?);
        if expected_postconditions != canonical_json(&row.postconditions) {
            bail!(
                "bad_bundle_history: postconditions changed for {}",
                row.migration_hash
            );
        }
        let failed_postconditions =
            self.failed_postconditions(&produced, &self.postconditions_for(&produced, &operation))?;
        if !failed_postconditions.is_empty() {
            bail!(
                "bad_bundle_history: migration {} failed postconditions",
                row.migration_hash
            );
        }

        let recomputed_history =
            history_hash(current_history.as_deref(), &row.migration_hash, &produced);
        if recomputed_history != row.history_hash {
            bail!(
                "bad_bundle_history: history {} recomputes to {}",
                row.history_hash,
                recomputed_history
            );
        }

        self.insert_migration_row(row)?;
        *current_root = produced;
        *current_history = Some(row.history_hash.clone());
        Ok(())
    }

    fn insert_migration_row(&mut self, row: &BundleMigration) -> Result<()> {
        let operation_json = canonical_json(&row.operation);
        let preconditions_json = canonical_json(&row.preconditions);
        let postconditions_json = canonical_json(&row.postconditions);
        let agent_json = canonical_json(&row.agent);
        let existing = self
            .conn
            .query_row(
                "SELECT parent_history_hash, input_root_hash, output_root_hash,
                        operation_kind, operation_json, preconditions_json,
                        postconditions_json, agent_json
                 FROM migrations WHERE hash = ?1",
                params![&row.migration_hash],
                |db_row| {
                    Ok((
                        db_row.get::<_, Option<String>>(0)?,
                        db_row.get::<_, String>(1)?,
                        db_row.get::<_, String>(2)?,
                        db_row.get::<_, String>(3)?,
                        db_row.get::<_, String>(4)?,
                        db_row.get::<_, String>(5)?,
                        db_row.get::<_, String>(6)?,
                        db_row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing
                != (
                    row.parent_history_hash.clone(),
                    row.input_root_hash.clone(),
                    row.output_root_hash.clone(),
                    row.operation_kind.clone(),
                    operation_json.clone(),
                    preconditions_json.clone(),
                    postconditions_json.clone(),
                    agent_json.clone(),
                )
            {
                bail!(
                    "bad_bundle_conflict: existing migration {} has different content",
                    row.migration_hash
                );
            }
        } else {
            self.conn.execute(
                "INSERT INTO migrations
                 (hash, parent_history_hash, input_root_hash, output_root_hash,
                  operation_kind, operation_json, preconditions_json, postconditions_json,
                  agent_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    &row.migration_hash,
                    row.parent_history_hash.as_deref(),
                    &row.input_root_hash,
                    &row.output_root_hash,
                    &row.operation_kind,
                    &operation_json,
                    &preconditions_json,
                    &postconditions_json,
                    &agent_json,
                ],
            )?;
        }

        let existing_history = self
            .conn
            .query_row(
                "SELECT parent_history_hash, migration_hash, output_root_hash
                 FROM histories WHERE history_hash = ?1",
                params![&row.history_hash],
                |db_row| {
                    Ok((
                        db_row.get::<_, Option<String>>(0)?,
                        db_row.get::<_, String>(1)?,
                        db_row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some(existing_history) = existing_history {
            if existing_history
                != (
                    row.parent_history_hash.clone(),
                    row.migration_hash.clone(),
                    row.output_root_hash.clone(),
                )
            {
                bail!(
                    "bad_bundle_conflict: existing history {} has different content",
                    row.history_hash
                );
            }
        } else {
            self.conn.execute(
                "INSERT INTO histories
                 (history_hash, parent_history_hash, migration_hash, output_root_hash)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    &row.history_hash,
                    row.parent_history_hash.as_deref(),
                    &row.migration_hash,
                    &row.output_root_hash,
                ],
            )?;
        }
        Ok(())
    }

    fn import_bundle_artifact(&mut self, artifact: &BundleArtifact) -> Result<()> {
        if artifact.schema != BUNDLE_ARTIFACT_SCHEMA {
            bail!("bad_bundle_artifact: unsupported artifact schema");
        }
        let key_input: CacheKeyInput = serde_json::from_value(artifact.cache_key_input.clone())
            .with_context(|| {
                format!(
                    "bad_bundle_artifact: invalid cache key {}",
                    artifact.cache_key
                )
            })?;
        let recomputed_key = cache_key_for_input(&key_input)?;
        if recomputed_key != artifact.cache_key {
            bail!(
                "bad_bundle_artifact: cache key {} recomputes to {}",
                artifact.cache_key,
                recomputed_key
            );
        }
        let artifact_bytes = artifact
            .artifact_bytes_hex
            .as_deref()
            .map(hex::decode)
            .transpose()
            .with_context(|| {
                format!(
                    "bad_bundle_artifact: artifact {} bytes are not hex",
                    artifact.cache_key
                )
            })?;
        validate_bundle_artifact_payload(&key_input, artifact, artifact_bytes.as_deref())?;
        self.write_cache_entry(
            &key_input,
            &artifact.artifact_hash,
            artifact.artifact_json.as_ref(),
            artifact_bytes.as_deref(),
        )
    }
}

fn validate_bundle_artifact_payload(
    key_input: &CacheKeyInput,
    artifact: &BundleArtifact,
    artifact_bytes: Option<&[u8]>,
) -> Result<()> {
    if let Some(artifact_json) = artifact.artifact_json.as_ref() {
        validate_bundle_artifact_metadata(key_input, artifact, artifact_json, artifact_bytes)
    } else if let Some(bytes) = artifact_bytes {
        if !key_input.artifact_kind.requires_artifact_bytes() {
            bail!(
                "bad_bundle_artifact: artifact {} stores bytes without metadata for non-byte artifact kind {}",
                artifact.cache_key,
                key_input.artifact_kind
            );
        }
        validate_bundle_artifact_bytes_hash(artifact, bytes)
    } else {
        bail!(
            "bad_bundle_artifact: artifact {} has neither metadata nor bytes to validate",
            artifact.cache_key
        );
    }
}

fn validate_bundle_artifact_metadata(
    key_input: &CacheKeyInput,
    artifact: &BundleArtifact,
    artifact_json: &JsonValue,
    artifact_bytes: Option<&[u8]>,
) -> Result<()> {
    if artifact_json.get("schema").and_then(JsonValue::as_str) != Some(ARTIFACT_METADATA_SCHEMA) {
        bail!(
            "bad_bundle_artifact: artifact {} metadata schema mismatch",
            artifact.cache_key
        );
    }
    if artifact_json
        .get("artifact_kind")
        .and_then(JsonValue::as_str)
        != Some(key_input.artifact_kind.as_str())
    {
        bail!(
            "bad_bundle_artifact: artifact {} kind does not match cache key",
            artifact.cache_key
        );
    }
    if artifact_json.get("input_hash").and_then(JsonValue::as_str)
        != Some(key_input.input_hash.as_str())
    {
        bail!(
            "bad_bundle_artifact: artifact {} input does not match cache key",
            artifact.cache_key
        );
    }
    if artifact_json.get("backend_id").and_then(JsonValue::as_str)
        != Some(key_input.backend_id.as_str())
    {
        bail!(
            "bad_bundle_artifact: artifact {} backend does not match cache key",
            artifact.cache_key
        );
    }
    if artifact_json
        .get("target_triple")
        .and_then(JsonValue::as_str)
        != Some(key_input.target_triple.as_str())
    {
        bail!(
            "bad_bundle_artifact: artifact {} target does not match cache key",
            artifact.cache_key
        );
    }

    match artifact_json
        .get("content_kind")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
    {
        "text" => {
            let text = artifact_json
                .get("text")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    anyhow!(
                        "bad_bundle_artifact: text artifact {} missing text",
                        artifact.cache_key
                    )
                })?;
            let recomputed = hash_bytes(BYTES_DOMAIN, text.as_bytes());
            validate_bundle_artifact_hash(artifact, &recomputed)?;
            if artifact_json.get("text_hash").and_then(JsonValue::as_str)
                != Some(artifact.artifact_hash.as_str())
            {
                bail!(
                    "bad_bundle_artifact: text artifact {} metadata hash mismatch",
                    artifact.cache_key
                );
            }
        }
        "json" => {
            let metadata = artifact_json.get("metadata").ok_or_else(|| {
                anyhow!(
                    "bad_bundle_artifact: JSON artifact {} missing metadata",
                    artifact.cache_key
                )
            })?;
            let recomputed = hash_bytes(BYTES_DOMAIN, canonical_json(metadata).as_bytes());
            validate_bundle_artifact_hash(artifact, &recomputed)?;
            if artifact_json
                .get("metadata_hash")
                .and_then(JsonValue::as_str)
                != Some(artifact.artifact_hash.as_str())
            {
                bail!(
                    "bad_bundle_artifact: JSON artifact {} metadata hash mismatch",
                    artifact.cache_key
                );
            }
        }
        "bytes" => {
            let bytes = artifact_bytes.ok_or_else(|| {
                anyhow!(
                    "bad_bundle_artifact: bytes artifact {} missing bytes",
                    artifact.cache_key
                )
            })?;
            validate_bundle_artifact_bytes_hash(artifact, bytes)?;
            if artifact_json.get("bytes_hash").and_then(JsonValue::as_str)
                != Some(artifact.artifact_hash.as_str())
            {
                bail!(
                    "bad_bundle_artifact: bytes artifact {} metadata hash mismatch",
                    artifact.cache_key
                );
            }
        }
        other => bail!(
            "bad_bundle_artifact: artifact {} has unknown content kind {other:?}",
            artifact.cache_key
        ),
    }
    Ok(())
}

fn validate_bundle_artifact_bytes_hash(artifact: &BundleArtifact, bytes: &[u8]) -> Result<()> {
    let recomputed_artifact = hash_bytes(BYTES_DOMAIN, bytes);
    validate_bundle_artifact_hash(artifact, &recomputed_artifact)
}

fn validate_bundle_artifact_hash(artifact: &BundleArtifact, recomputed: &str) -> Result<()> {
    if recomputed != artifact.artifact_hash {
        bail!(
            "bad_bundle_artifact: artifact {} recomputes to {}",
            artifact.artifact_hash,
            recomputed
        );
    }
    Ok(())
}

fn parse_bundle_document(text: &str) -> Result<BundleDocument> {
    let canonical_source = text
        .strip_suffix('\n')
        .ok_or_else(|| anyhow!("bad_bundle: bundle JSON must end with a single newline"))?;
    if canonical_source.ends_with('\n') {
        bail!("bad_bundle: bundle JSON must end with a single newline");
    }
    let value: JsonValue =
        serde_json::from_str(canonical_source).context("bad_bundle: invalid JSON")?;
    let canonical = canonical_json(&value);
    if canonical != canonical_source {
        bail!("bad_bundle: bundle JSON is not canonical");
    }
    serde_json::from_value(value).context("bad_bundle: invalid bundle document")
}

fn validate_bundle_manifest(document: &BundleDocument) -> Result<()> {
    if document.schema != BUNDLE_SCHEMA {
        bail!("bad_bundle: unsupported bundle schema {}", document.schema);
    }
    let manifest = &document.manifest;
    if manifest.schema != BUNDLE_MANIFEST_SCHEMA {
        bail!("bad_bundle: unsupported bundle manifest schema");
    }
    if manifest.api_hash != bundle_api_hash() {
        bail!(
            "bad_bundle: unsupported bundle API hash {}",
            manifest.api_hash
        );
    }
    let expected_package = package_hash(
        &manifest.root_hash,
        manifest.history_hash.as_deref(),
        &manifest.api_hash,
    );
    if manifest.package_hash != expected_package {
        bail!(
            "bad_bundle: package hash {} recomputes to {}",
            manifest.package_hash,
            expected_package
        );
    }
    if manifest.object_count != document.objects.len() {
        bail!(
            "bad_bundle: manifest object_count {} does not match {}",
            manifest.object_count,
            document.objects.len()
        );
    }
    if manifest.migration_count != document.migrations.len() {
        bail!(
            "bad_bundle: manifest migration_count {} does not match {}",
            manifest.migration_count,
            document.migrations.len()
        );
    }
    if manifest.artifact_count != document.artifact_cache.len() {
        bail!(
            "bad_bundle: manifest artifact_count {} does not match {}",
            manifest.artifact_count,
            document.artifact_cache.len()
        );
    }
    if !manifest.artifact_cache_included && !document.artifact_cache.is_empty() {
        bail!("bad_bundle: manifest omits artifact cache but artifact_cache is present");
    }
    if manifest.requires_projection_sources {
        bail!("bad_bundle: projection source files are not supported as bundle inputs");
    }
    if !document
        .objects
        .iter()
        .any(|object| object.hash == manifest.root_hash)
    {
        bail!(
            "bad_bundle: root object {} is missing from object closure",
            manifest.root_hash
        );
    }
    match (manifest.history_hash.as_deref(), document.migrations.last()) {
        (Some(expected), Some(last)) if expected == last.history_hash => {}
        (Some(expected), _) => {
            bail!("bad_bundle_history: history head {expected} is missing from migration slice")
        }
        (None, None) => {}
        (None, Some(_)) => bail!("bad_bundle_history: migration slice has no manifest history"),
    }
    Ok(())
}

fn validate_bundle_objects(document: &BundleDocument) -> Result<BTreeMap<String, BundleObject>> {
    let mut objects = BTreeMap::new();
    for object in &document.objects {
        if object.schema != BUNDLE_OBJECT_SCHEMA {
            bail!("bad_bundle: unsupported object schema for {}", object.hash);
        }
        let canonical_payload = canonical_json(&object.payload);
        let recomputed =
            hash_object_canonical(&object.kind, object.schema_version, &canonical_payload);
        if recomputed != object.hash {
            bail!(
                "bad_bundle_hash: object {} recomputes to {}",
                object.hash,
                recomputed
            );
        }
        if objects
            .insert(object.hash.clone(), object.clone())
            .is_some()
        {
            bail!("bad_bundle: duplicate object {}", object.hash);
        }
    }
    Ok(objects)
}

fn validate_bundle_object_closure(
    document: &BundleDocument,
    objects_by_hash: &BTreeMap<String, BundleObject>,
) -> Result<()> {
    let mut roots = BTreeSet::from([document.manifest.root_hash.clone()]);
    for migration in &document.migrations {
        roots.insert(migration.input_root_hash.clone());
        roots.insert(migration.output_root_hash.clone());
    }
    let root_closure = bundle_object_closure_from_map(&roots, objects_by_hash)?;
    let expected = expected_bundle_object_set(document, objects_by_hash, &root_closure)?;
    let actual = objects_by_hash.keys().cloned().collect::<BTreeSet<_>>();
    if expected != actual {
        let missing = expected
            .difference(&actual)
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        let extra = actual
            .difference(&expected)
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        bail!(
            "bad_bundle_closure: object set does not match root closure; missing [{}], extra [{}]",
            missing,
            extra
        );
    }
    Ok(())
}

fn expected_bundle_object_set(
    document: &BundleDocument,
    objects_by_hash: &BTreeMap<String, BundleObject>,
    root_closure: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut expected = root_closure.clone();
    if document.manifest.artifact_cache_included {
        for artifact in &document.artifact_cache {
            let key_input: CacheKeyInput = serde_json::from_value(artifact.cache_key_input.clone())
                .with_context(|| {
                    format!(
                        "bad_bundle_artifact: invalid cache key {}",
                        artifact.cache_key
                    )
                })?;
            if root_closure.contains(&key_input.input_hash) {
                continue;
            }
            let object = objects_by_hash.get(&key_input.input_hash).ok_or_else(|| {
                anyhow!(
                    "bad_bundle_closure: missing artifact input object {}",
                    key_input.input_hash
                )
            })?;
            validate_artifact_input_object(object)?;
            expected.insert(key_input.input_hash);
        }
    }
    let expected_closure = bundle_object_closure_from_map(&expected, objects_by_hash)?;
    for hash in expected_closure.difference(root_closure) {
        let object = objects_by_hash
            .get(hash)
            .ok_or_else(|| anyhow!("bad_bundle_closure: missing artifact input object {hash}"))?;
        validate_artifact_input_object(object)?;
    }
    Ok(expected_closure)
}

fn validate_artifact_input_object(object: &BundleObject) -> Result<()> {
    if !artifact_input_object_kind_is_supported(object) {
        bail!(
            "bad_bundle_closure: artifact input object {} has unsupported kind {}",
            object.hash,
            object.kind
        );
    }
    Ok(())
}

fn artifact_input_object_kind_is_supported(object: &BundleObject) -> bool {
    match object.kind.as_str() {
        "FunctionInterface" => true,
        "Type" => true,
        "LinkPlanInput" => {
            object.payload.get("schema").and_then(JsonValue::as_str) == Some("codedb/link-input/v1")
        }
        _ => false,
    }
}

fn bundle_object_closure_from_map(
    roots: &BTreeSet<String>,
    objects_by_hash: &BTreeMap<String, BundleObject>,
) -> Result<BTreeSet<String>> {
    let mut seen = BTreeSet::new();
    let mut frontier = roots.iter().cloned().collect::<Vec<_>>();
    while let Some(hash) = frontier.pop() {
        if !seen.insert(hash.clone()) {
            continue;
        }
        let object = objects_by_hash
            .get(&hash)
            .ok_or_else(|| anyhow!("bad_bundle_closure: missing object {hash}"))?;
        let mut refs = Vec::new();
        collect_bundle_object_refs(&object.kind, &object.payload, &mut refs);
        for child_hash in refs {
            if !objects_by_hash.contains_key(&child_hash) {
                bail!("bad_bundle_closure: missing object {child_hash} referenced by {hash}");
            }
            if !seen.contains(&child_hash) {
                frontier.push(child_hash);
            }
        }
    }
    Ok(seen)
}

fn collect_bundle_object_refs(kind: &str, payload: &JsonValue, refs: &mut Vec<String>) {
    match kind {
        "Type" => match payload.get("type_kind").and_then(JsonValue::as_str) {
            Some("Named") => {
                push_hash_ref(payload.get("type_symbol"), refs);
                push_hash_array_refs(payload.get("region_args"), refs);
            }
            Some("Reference") => {
                push_hash_ref(payload.get("region"), refs);
                push_hash_ref(payload.get("referent"), refs);
            }
            Some("RawPointer") => {
                push_hash_ref(payload.get("pointee"), refs);
            }
            Some("Box") => {
                push_hash_ref(payload.get("element"), refs);
            }
            Some("Slice") => {
                push_hash_ref(payload.get("region"), refs);
                push_hash_ref(payload.get("element"), refs);
            }
            Some("FixedArray") => {
                push_hash_ref(payload.get("element"), refs);
            }
            Some("Record") => {
                for field in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                {
                    push_hash_ref(field.get("type"), refs);
                }
            }
            Some("Enum") => {
                for variant in payload
                    .get("variants")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                {
                    push_hash_ref(variant.get("type"), refs);
                }
            }
            _ => {}
        },
        "SymbolBirth" => {}
        "FunctionSignature" => {
            for param in payload
                .get("region_params")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(param.get("region"), refs);
            }
            push_hash_array_refs(payload.get("params"), refs);
            push_hash_ref(payload.get("return"), refs);
        }
        "Expression" => {
            push_hash_ref(payload.get("type"), refs);
            match payload.get("expr_kind").and_then(JsonValue::as_str) {
                Some("static_bytes") => {
                    push_hash_ref(payload.get("static_data"), refs);
                    push_hash_ref(payload.get("region"), refs);
                    push_hash_ref(payload.get("element_type"), refs);
                }
                Some("call") => {
                    push_hash_ref(payload.get("symbol"), refs);
                    push_hash_array_refs(payload.get("args"), refs);
                }
                Some("binary") => {
                    push_hash_ref(payload.get("left"), refs);
                    push_hash_ref(payload.get("right"), refs);
                }
                Some("unary") => push_hash_ref(payload.get("expr"), refs),
                Some("borrow_shared" | "borrow_mut") => {
                    push_hash_ref(payload.get("target"), refs);
                    push_hash_ref(payload.get("region"), refs);
                    push_hash_ref(payload.get("referent_type"), refs);
                }
                Some("slice_from_array") => {
                    push_hash_ref(payload.get("target"), refs);
                    push_hash_ref(payload.get("target_type"), refs);
                    push_hash_ref(payload.get("array_type"), refs);
                    push_hash_ref(payload.get("element_type"), refs);
                    push_hash_ref(payload.get("region"), refs);
                }
                Some("slice_len") => {
                    push_hash_ref(payload.get("target"), refs);
                    push_hash_ref(payload.get("slice_type"), refs);
                }
                Some("subslice") => {
                    push_hash_ref(payload.get("target"), refs);
                    push_hash_ref(payload.get("start"), refs);
                    push_hash_ref(payload.get("len"), refs);
                    push_hash_ref(payload.get("slice_type"), refs);
                    push_hash_ref(payload.get("element_type"), refs);
                }
                Some("box_new") => {
                    push_hash_ref(payload.get("value"), refs);
                    push_hash_ref(payload.get("element_type"), refs);
                }
                Some("unbox") => {
                    push_hash_ref(payload.get("value"), refs);
                    push_hash_ref(payload.get("element_type"), refs);
                    push_hash_ref(payload.get("box_type"), refs);
                }
                Some("int_cast") => {
                    push_hash_ref(payload.get("value"), refs);
                    push_hash_ref(payload.get("source_type"), refs);
                    push_hash_ref(payload.get("type"), refs);
                }
                Some("raw_ptr_cast") => {
                    push_hash_ref(payload.get("value"), refs);
                    push_hash_ref(payload.get("source_type"), refs);
                    push_hash_ref(payload.get("pointee_type"), refs);
                }
                Some("raw_load") => {
                    push_hash_ref(payload.get("pointer"), refs);
                    push_hash_ref(payload.get("pointer_type"), refs);
                    push_hash_ref(payload.get("pointee_type"), refs);
                }
                Some("raw_store") => {
                    push_hash_ref(payload.get("pointer"), refs);
                    push_hash_ref(payload.get("value"), refs);
                    push_hash_ref(payload.get("pointer_type"), refs);
                    push_hash_ref(payload.get("pointee_type"), refs);
                }
                Some("assign") => {
                    push_hash_ref(payload.get("target"), refs);
                    push_hash_ref(payload.get("value"), refs);
                    push_hash_ref(payload.get("target_type"), refs);
                }
                Some("let") => {
                    push_hash_ref(payload.get("binding_type"), refs);
                    push_hash_ref(payload.get("value"), refs);
                    push_hash_ref(payload.get("body"), refs);
                }
                Some("if") => {
                    push_hash_ref(payload.get("cond"), refs);
                    push_hash_ref(payload.get("then"), refs);
                    push_hash_ref(payload.get("else"), refs);
                }
                Some("fold") => {
                    push_hash_ref(payload.get("target"), refs);
                    push_hash_ref(payload.get("target_type"), refs);
                    push_hash_ref(payload.get("element_type"), refs);
                    push_hash_ref(payload.get("init"), refs);
                    push_hash_ref(payload.get("acc_type"), refs);
                    push_hash_ref(payload.get("body"), refs);
                }
                Some("record_literal") => {
                    for field in payload
                        .get("fields")
                        .and_then(JsonValue::as_array)
                        .into_iter()
                        .flatten()
                    {
                        push_hash_ref(field.get("value"), refs);
                        push_hash_ref(field.get("type"), refs);
                    }
                }
                Some("array_literal") => {
                    push_hash_ref(payload.get("element_type"), refs);
                    for element in payload
                        .get("elements")
                        .and_then(JsonValue::as_array)
                        .into_iter()
                        .flatten()
                    {
                        push_hash_ref(element.get("value"), refs);
                        push_hash_ref(element.get("type"), refs);
                    }
                }
                Some("array_index") => {
                    push_hash_ref(payload.get("target"), refs);
                    push_hash_ref(payload.get("index"), refs);
                    push_hash_ref(payload.get("target_type"), refs);
                    push_hash_ref(payload.get("array_type"), refs);
                    push_hash_ref(payload.get("element_type"), refs);
                }
                Some("field_access") => push_hash_ref(payload.get("target"), refs),
                Some("enum_construct") => {
                    push_hash_ref(payload.get("enum_type"), refs);
                    push_hash_ref(payload.get("value"), refs);
                }
                Some("case") => {
                    push_hash_ref(payload.get("expr"), refs);
                    for arm in payload
                        .get("arms")
                        .and_then(JsonValue::as_array)
                        .into_iter()
                        .flatten()
                    {
                        // The guard (R14) is a referenced typed-DAG node; it must be
                        // bundled alongside the arm body or the bundle is incomplete.
                        push_hash_ref(arm.get("guard"), refs);
                        push_hash_ref(arm.get("body"), refs);
                    }
                }
                _ => {}
            }
        }
        "FunctionDef" => {
            push_hash_ref(payload.get("symbol"), refs);
            push_hash_ref(payload.get("function_sig_hash"), refs);
            push_hash_ref(payload.get("typed_body_expr_hash"), refs);
        }
        "ExternalFunction" => {
            push_hash_ref(payload.get("symbol"), refs);
            push_hash_ref(payload.get("function_sig_hash"), refs);
        }
        "FunctionInterface" => {
            push_hash_ref(payload.get("symbol_hash"), refs);
            push_hash_ref(payload.get("signature_hash"), refs);
        }
        "ProgramRoot" => {
            for entry in payload
                .get("symbols")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(entry.get("symbol"), refs);
                push_hash_ref(entry.get("definition"), refs);
                push_hash_ref(entry.get("signature"), refs);
            }
            for binding in payload
                .get("names")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(binding.get("symbol"), refs);
            }
            for entry in payload
                .get("types")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(entry.get("type_symbol"), refs);
                push_hash_ref(entry.get("type_def"), refs);
            }
            for binding in payload
                .get("type_names")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(binding.get("type_symbol"), refs);
            }
            for entry in payload
                .get("param_names")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(entry.get("symbol"), refs);
            }
            for binding in payload
                .get("exports")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(binding.get("symbol"), refs);
            }
            for binding in payload
                .get("tests")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(binding.get("test"), refs);
            }
            for group in payload
                .get("recursion_groups")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(Some(group), refs);
            }
            for group in payload
                .get("type_recursion_groups")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(Some(group), refs);
            }
        }
        "RecursionGroup" => {
            for member in payload
                .get("members")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(member.get("symbol"), refs);
                push_hash_ref(member.get("definition"), refs);
                push_hash_ref(member.get("signature"), refs);
            }
        }
        "TypeRecursionGroup" => {
            for member in payload
                .get("members")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(member.get("type_symbol"), refs);
                push_hash_ref(member.get("type_def"), refs);
            }
        }
        "TestCase" => push_hash_ref(payload.get("entry_symbol"), refs),
        "LinkPlanInput" => {
            push_hash_ref(payload.get("entry_symbol_hash"), refs);
            for entry in payload
                .get("export_map")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(entry.get("symbol_hash"), refs);
            }
            for entry in payload
                .get("external_symbols")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                push_hash_ref(entry.get("symbol_hash"), refs);
                push_hash_ref(entry.get("definition_hash"), refs);
                push_hash_ref(entry.get("signature_hash"), refs);
                push_hash_array_refs(entry.get("param_type_hashes"), refs);
                push_hash_ref(entry.get("return_type_hash"), refs);
            }
        }
        _ => extract_hash_strings(payload, refs),
    }
}

fn push_hash_ref(value: Option<&JsonValue>, refs: &mut Vec<String>) {
    if let Some(hash) = value.and_then(JsonValue::as_str) {
        refs.push(hash.to_string());
    }
}

fn push_hash_array_refs(value: Option<&JsonValue>, refs: &mut Vec<String>) {
    for item in value.and_then(JsonValue::as_array).into_iter().flatten() {
        push_hash_ref(Some(item), refs);
    }
}

fn bundle_migration_audit_hash(row: &BundleMigration) -> Result<String> {
    Ok(hash_bytes(
        BYTES_DOMAIN,
        canonical_json(&json!({
            "schema": BUNDLE_MIGRATION_AUDIT_SCHEMA,
            "sequence": row.sequence,
            "migration_hash": &row.migration_hash,
            "history_hash": &row.history_hash,
            "parent_history_hash": &row.parent_history_hash,
            "input_root_hash": &row.input_root_hash,
            "output_root_hash": &row.output_root_hash,
            "operation_kind": &row.operation_kind,
            "operation": &row.operation,
            "preconditions": &row.preconditions,
            "postconditions": &row.postconditions,
            "agent": &row.agent,
        }))
        .as_bytes(),
    ))
}

fn bundle_api_hash() -> String {
    hash_bytes(
        BYTES_DOMAIN,
        canonical_json(&json!({
            "schema": BUNDLE_API_SCHEMA,
            "bundle_schema": BUNDLE_SCHEMA,
            "manifest_schema": BUNDLE_MANIFEST_SCHEMA,
            "object_schema": BUNDLE_OBJECT_SCHEMA,
            "migration_schema": BUNDLE_MIGRATION_SCHEMA,
            "migration_audit_schema": BUNDLE_MIGRATION_AUDIT_SCHEMA,
            "artifact_schema": BUNDLE_ARTIFACT_SCHEMA,
        }))
        .as_bytes(),
    )
}

fn package_hash(root_hash: &str, history_hash: Option<&str>, api_hash: &str) -> String {
    hash_bytes(
        BYTES_DOMAIN,
        canonical_json(&json!({
            "schema": PACKAGE_IDENTITY_SCHEMA,
            "root_hash": root_hash,
            "history_hash": history_hash,
            "api_hash": api_hash,
        }))
        .as_bytes(),
    )
}
