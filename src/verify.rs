use std::collections::BTreeSet;

use anyhow::{Result, bail};
use rusqlite::params;
use serde_json::Value as JsonValue;

use crate::backend::ArtifactKind;
use crate::backend_c::ensure_no_forbidden_runtime_calls;
use crate::diff::dependency_pairs;
use crate::migrations::{history_hash, migration_hash};
use crate::model::ProgramRootPayload;
use crate::store::{CodeDb, canonical_json, hash_object_canonical};

impl CodeDb {
    pub fn verify(&mut self) -> Result<String> {
        self.ensure_initialized()?;
        let mut errors = Vec::new();
        self.verify_objects(&mut errors)?;
        self.verify_edges(&mut errors)?;
        self.verify_branches(&mut errors)?;
        self.verify_migrations_and_histories(&mut errors)?;
        self.verify_roots(&mut errors)?;
        self.verify_caches(&mut errors)?;
        if let Err(err) = self.replay_main_branch() {
            let message = format!("{err:#}");
            if message.starts_with("bad_history_link") || message.starts_with("semantic_conflict") {
                errors.push(message);
            } else {
                errors.push(format!("bad_history_link: {message}"));
            }
        }

        if errors.is_empty() {
            Ok("verify ok\n".to_string())
        } else {
            bail!("verify failed\n{}", errors.join("\n"));
        }
    }

    fn verify_objects(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT hash, kind, schema_version, payload_json FROM objects ORDER BY hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (hash, kind, schema_version, payload_json) = row?;
            match serde_json::from_str::<JsonValue>(&payload_json) {
                Ok(value) => {
                    let canonical = canonical_json(&value);
                    if canonical != payload_json {
                        errors.push(format!("corrupt_object: payload is not canonical {hash}"));
                    }
                    let recomputed = hash_object_canonical(&kind, schema_version, &canonical);
                    if recomputed != hash {
                        errors.push(format!("bad_hash: {hash} recomputes to {recomputed}"));
                    }
                }
                Err(err) => errors.push(format!("corrupt_object: {hash}: {err}")),
            }
        }
        Ok(())
    }

    fn verify_edges(&self, errors: &mut Vec<String>) -> Result<()> {
        let missing_parent: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM object_edges e
             LEFT JOIN objects o ON o.hash = e.parent_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        let missing_child: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM object_edges e
             LEFT JOIN objects o ON o.hash = e.child_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        if missing_parent > 0 || missing_child > 0 {
            errors.push(format!(
                "missing_object: object_edges missing parents={missing_parent} children={missing_child}"
            ));
        }
        Ok(())
    }

    fn verify_branches(&self, errors: &mut Vec<String>) -> Result<()> {
        let missing_roots: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM branches b
             LEFT JOIN objects o ON o.hash = b.root_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        if missing_roots > 0 {
            errors.push(format!(
                "missing_object: branch roots missing {missing_roots}"
            ));
        }
        Ok(())
    }

    fn verify_migrations_and_histories(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT hash, parent_history_hash, input_root_hash, output_root_hash,
                    operation_json, preconditions_json, postconditions_json
             FROM migrations ORDER BY created_at, hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        for row in rows {
            let (
                hash,
                parent_history,
                input_root,
                output_root,
                operation_json,
                preconditions_json,
                postconditions_json,
            ) = row?;
            let operation = serde_json::from_str::<JsonValue>(&operation_json);
            let preconditions = serde_json::from_str::<JsonValue>(&preconditions_json);
            let postconditions = serde_json::from_str::<JsonValue>(&postconditions_json);
            match (operation, preconditions, postconditions) {
                (Ok(operation), Ok(preconditions), Ok(postconditions)) => {
                    let recomputed = migration_hash(
                        parent_history.as_deref(),
                        &input_root,
                        &output_root,
                        &operation,
                        &preconditions,
                        &postconditions,
                    );
                    if recomputed != hash {
                        errors.push(format!(
                            "bad_hash: migration {hash} recomputes to {recomputed}"
                        ));
                    }
                }
                _ => errors.push(format!("corrupt_object: migration json invalid {hash}")),
            }
            for root in [input_root, output_root] {
                let exists: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                    params![root],
                    |row| row.get(0),
                )?;
                if !exists {
                    errors.push(format!(
                        "missing_object: migration {hash} references missing root {root}"
                    ));
                }
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT history_hash, parent_history_hash, migration_hash, output_root_hash
             FROM histories ORDER BY created_at, history_hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (history, parent, migration, output_root) = row?;
            let recomputed = history_hash(parent.as_deref(), &migration, &output_root);
            if recomputed != history {
                errors.push(format!(
                    "bad_history_link: {history} recomputes to {recomputed}"
                ));
            }
        }
        Ok(())
    }

    fn verify_roots(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM objects WHERE kind = 'ProgramRoot' ORDER BY hash")?;
        let root_hashes = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        for root_hash in root_hashes {
            match self.type_check_root(&root_hash) {
                Ok(()) => {}
                Err(err) => errors.push(format!("bad_type: root {root_hash}: {err:#}")),
            }
            let root = match self.load_root(&root_hash) {
                Ok(root) => root,
                Err(err) => {
                    errors.push(format!("corrupt_object: root {root_hash}: {err:#}"));
                    continue;
                }
            };
            self.verify_root_indexes(&root_hash, &root, errors)?;
        }
        Ok(())
    }

    fn verify_root_indexes(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        let expected_symbols = root
            .symbols
            .iter()
            .map(|entry| {
                (
                    entry.symbol.clone(),
                    entry.definition.clone(),
                    entry.signature.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let actual_symbols = {
            let mut stmt = self.conn.prepare(
                "SELECT symbol_hash, definition_hash, signature_hash FROM root_symbols
                 WHERE root_hash = ?1 ORDER BY symbol_hash",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_symbols != actual_symbols {
            errors.push(format!("bad_index: root_symbols mismatch for {root_hash}"));
        }

        let expected_names = root
            .names
            .iter()
            .map(|binding| {
                (
                    binding.module.clone(),
                    binding.display_name.clone(),
                    binding.symbol.clone(),
                    binding.is_preferred,
                )
            })
            .collect::<BTreeSet<_>>();
        let actual_names = {
            let mut stmt = self.conn.prepare(
                "SELECT module_name, display_name, symbol_hash, is_preferred FROM root_names
                 WHERE root_hash = ?1 ORDER BY module_name, display_name",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_names != actual_names {
            errors.push(format!("bad_index: root_names mismatch for {root_hash}"));
        }

        let mut expected_deps = BTreeSet::new();
        for entry in &root.symbols {
            for dep in self.dependencies_for_definition(root, &entry.definition)? {
                expected_deps.insert((entry.symbol.clone(), dep));
            }
        }
        let actual_deps = dependency_pairs(&self.conn, root_hash)?;
        if expected_deps != actual_deps {
            errors.push(format!(
                "bad_dependency_index: dependencies mismatch for {root_hash}"
            ));
        }
        Ok(())
    }

    fn verify_caches(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT cache_key, input_hash, artifact_kind, artifact_json FROM compile_cache ORDER BY cache_key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (cache_key, input_hash, artifact_kind, artifact_json) = row?;
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                params![input_hash],
                |row| row.get(0),
            )?;
            if !exists {
                errors.push(format!(
                    "bad_cache_entry: {cache_key} references missing input {input_hash}"
                ));
            }
            let Some(artifact_kind) = ArtifactKind::from_str(&artifact_kind) else {
                errors.push(format!(
                    "bad_cache_entry: {cache_key} has unknown artifact kind {artifact_kind}"
                ));
                continue;
            };
            if artifact_kind == ArtifactKind::CProjection
                && let Some(artifact_json) = artifact_json
            {
                match serde_json::from_str::<JsonValue>(&artifact_json) {
                    Ok(value) => {
                        if let Some(text) = value.get("text").and_then(JsonValue::as_str)
                            && let Err(err) = ensure_no_forbidden_runtime_calls(text)
                        {
                            errors.push(format!(
                                "forbidden_runtime_dependency: {cache_key}: {err:#}"
                            ));
                        }
                    }
                    Err(err) => errors.push(format!("bad_cache_entry: {cache_key}: {err}")),
                }
            }
        }
        Ok(())
    }
}
