use std::collections::BTreeSet;

use anyhow::{Result, bail};
use rusqlite::params;
use serde_json::Value as JsonValue;

use crate::BYTES_DOMAIN;
use crate::abi::{export_map, validate_exported_abi_name};
use crate::artifact::{ARTIFACT_METADATA_SCHEMA, CacheKeyInput};
use crate::backend::ArtifactKind;
use crate::backend_c::ensure_no_forbidden_runtime_calls;
use crate::diff::dependency_pairs;
use crate::migrations::{history_hash, migration_hash};
use crate::model::ProgramRootPayload;
use crate::store::{
    CodeDb, cache_key_for_input, canonical_json, hash_bytes, hash_object_canonical,
};

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

        let root_symbols = root
            .symbols
            .iter()
            .map(|entry| entry.symbol.clone())
            .collect::<BTreeSet<_>>();
        let mut seen_exported_names = BTreeSet::new();
        for export in &root.exports {
            if !root_symbols.contains(&export.symbol) {
                errors.push(format!(
                    "bad_abi_symbol: export {} points to missing symbol {} in {root_hash}",
                    export.exported_name, export.symbol
                ));
            }
            if let Err(err) = validate_exported_abi_name(&export.exported_name) {
                errors.push(format!(
                    "bad_abi_symbol: invalid export {} in {root_hash}: {err:#}",
                    export.exported_name
                ));
            }
            if !seen_exported_names.insert(export.exported_name.clone()) {
                errors.push(format!(
                    "bad_abi_symbol: duplicate export {} in {root_hash}",
                    export.exported_name
                ));
            }
        }
        if let Err(err) = export_map(root) {
            errors.push(format!("bad_abi_symbol: root {root_hash}: {err:#}"));
        }

        let expected_exports = root
            .exports
            .iter()
            .map(|binding| (binding.exported_name.clone(), binding.symbol.clone()))
            .collect::<BTreeSet<_>>();
        let actual_exports = {
            let mut stmt = self.conn.prepare(
                "SELECT exported_name, symbol_hash FROM root_exports
                 WHERE root_hash = ?1 ORDER BY exported_name",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_exports != actual_exports {
            errors.push(format!("bad_index: root_exports mismatch for {root_hash}"));
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
            "SELECT cache_key, cache_key_json, input_hash, backend, target, compiler_version,
                    artifact_kind, artifact_hash, artifact_json, artifact_bytes
             FROM compile_cache ORDER BY cache_key",
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
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<Vec<u8>>>(9)?,
            ))
        })?;
        for row in rows {
            let (
                cache_key,
                cache_key_json,
                input_hash,
                backend,
                target,
                compiler_version,
                artifact_kind,
                artifact_hash,
                artifact_json,
                artifact_bytes,
            ) = row?;
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                params![&input_hash],
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

            match cache_key_json {
                Some(cache_key_json) => match serde_json::from_str::<JsonValue>(&cache_key_json) {
                    Ok(value) => {
                        if canonical_json(&value) != cache_key_json {
                            errors.push(format!(
                                "bad_cache_entry: cache key payload is not canonical {cache_key}"
                            ));
                        }
                        match serde_json::from_value::<CacheKeyInput>(value) {
                            Ok(key_input) => {
                                let key_input = key_input.normalized();
                                if let Err(err) = key_input.validate() {
                                    errors.push(format!(
                                        "bad_cache_entry: invalid cache key payload {cache_key}: {err:#}"
                                    ));
                                } else {
                                    if key_input.artifact_kind != artifact_kind {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key artifact kind mismatch {cache_key}"
                                        ));
                                    }
                                    if key_input.input_hash != input_hash {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key input mismatch {cache_key}"
                                        ));
                                    }
                                    if key_input.backend_id != backend {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key backend mismatch {cache_key}"
                                        ));
                                    }
                                    if key_input.target_triple != target {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key target mismatch {cache_key}"
                                        ));
                                    }
                                    if key_input.compiler_version != compiler_version {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key compiler version mismatch {cache_key}"
                                        ));
                                    }
                                    match cache_key_for_input(&key_input) {
                                        Ok(recomputed) if recomputed != cache_key => {
                                            errors.push(format!(
                                                "bad_cache_entry: cache key mismatch {cache_key} recomputes to {recomputed}"
                                            ));
                                        }
                                        Ok(_) => {}
                                        Err(err) => errors.push(format!(
                                            "bad_cache_entry: cannot recompute cache key {cache_key}: {err:#}"
                                        )),
                                    }
                                }
                            }
                            Err(err) => errors.push(format!(
                                "bad_cache_entry: invalid cache key json {cache_key}: {err}"
                            )),
                        }
                    }
                    Err(err) => errors.push(format!(
                        "bad_cache_entry: invalid cache key json {cache_key}: {err}"
                    )),
                },
                None => errors.push(format!(
                    "bad_cache_entry: missing cache key payload {cache_key}"
                )),
            }

            let artifact_value = match artifact_json.as_deref() {
                Some(artifact_json) => match serde_json::from_str::<JsonValue>(artifact_json) {
                    Ok(value) => {
                        if canonical_json(&value) != artifact_json {
                            errors.push(format!(
                                "bad_cache_entry: artifact_json is not canonical {cache_key}"
                            ));
                        }
                        Some(value)
                    }
                    Err(err) => {
                        errors.push(format!("bad_cache_entry: {cache_key}: {err}"));
                        None
                    }
                },
                None => None,
            };

            if artifact_kind.requires_artifact_bytes() && artifact_bytes.is_none() {
                errors.push(format!(
                    "bad_artifact_bytes: {cache_key} missing artifact bytes for {artifact_kind}"
                ));
            }

            if let Some(value) = artifact_value.as_ref() {
                verify_artifact_metadata(
                    errors,
                    &cache_key,
                    artifact_kind,
                    &input_hash,
                    &artifact_hash,
                    value,
                    artifact_bytes.as_deref(),
                );
            } else if let Some(bytes) = artifact_bytes.as_deref() {
                let recomputed = hash_bytes(BYTES_DOMAIN, bytes);
                if recomputed != artifact_hash {
                    errors.push(format!(
                        "bad_artifact_bytes: {cache_key} artifact bytes hash {artifact_hash} recomputes to {recomputed}"
                    ));
                }
            }

            if artifact_kind == ArtifactKind::CProjection
                && let Some(value) = artifact_value.as_ref()
                && let Some(text) = artifact_text(value)
                && let Err(err) = ensure_no_forbidden_runtime_calls(text)
            {
                errors.push(format!(
                    "forbidden_runtime_dependency: {cache_key}: {err:#}"
                ));
            }

            if artifact_kind == ArtifactKind::LoweredIr
                && let Some(value) = artifact_value.as_ref()
            {
                match crate::lowering::lowered_ir_from_artifact_metadata(value) {
                    Ok(ir) => {
                        if let Err(err) = self.verify_lowered_ir_against_index(&input_hash, &ir) {
                            errors.push(format!("bad_lowered_ir: {cache_key}: {err:#}"));
                        }
                    }
                    Err(err) => errors.push(format!("bad_lowered_ir: {cache_key}: {err:#}")),
                }
            }
        }
        Ok(())
    }
}

fn verify_artifact_metadata(
    errors: &mut Vec<String>,
    cache_key: &str,
    artifact_kind: ArtifactKind,
    input_hash: &str,
    artifact_hash: &str,
    artifact_json: &JsonValue,
    artifact_bytes: Option<&[u8]>,
) {
    if artifact_json
        .get("schema")
        .and_then(JsonValue::as_str)
        .is_some_and(|schema| schema != ARTIFACT_METADATA_SCHEMA)
    {
        errors.push(format!(
            "bad_cache_entry: artifact metadata schema mismatch {cache_key}"
        ));
    }
    if artifact_json.get("schema").and_then(JsonValue::as_str) != Some(ARTIFACT_METADATA_SCHEMA) {
        return;
    }
    if artifact_json
        .get("artifact_kind")
        .and_then(JsonValue::as_str)
        != Some(artifact_kind.as_str())
    {
        errors.push(format!(
            "bad_cache_entry: artifact metadata kind mismatch {cache_key}"
        ));
    }
    if artifact_json.get("input_hash").and_then(JsonValue::as_str) != Some(input_hash) {
        errors.push(format!(
            "bad_cache_entry: artifact metadata input mismatch {cache_key}"
        ));
    }

    match artifact_json
        .get("content_kind")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
    {
        "text" => {
            let Some(text) = artifact_json.get("text").and_then(JsonValue::as_str) else {
                errors.push(format!(
                    "bad_cache_entry: text artifact missing text {cache_key}"
                ));
                return;
            };
            let recomputed = hash_bytes(BYTES_DOMAIN, text.as_bytes());
            if recomputed != artifact_hash {
                errors.push(format!(
                    "bad_cache_entry: artifact text hash {artifact_hash} recomputes to {recomputed}"
                ));
            }
            if artifact_json.get("text_hash").and_then(JsonValue::as_str) != Some(artifact_hash) {
                errors.push(format!(
                    "bad_cache_entry: text artifact metadata hash mismatch {cache_key}"
                ));
            }
        }
        "json" => {
            let Some(metadata) = artifact_json.get("metadata") else {
                errors.push(format!(
                    "bad_cache_entry: json artifact missing metadata {cache_key}"
                ));
                return;
            };
            let recomputed = hash_bytes(BYTES_DOMAIN, canonical_json(metadata).as_bytes());
            if recomputed != artifact_hash {
                errors.push(format!(
                    "bad_cache_entry: artifact json hash {artifact_hash} recomputes to {recomputed}"
                ));
            }
            if artifact_json
                .get("metadata_hash")
                .and_then(JsonValue::as_str)
                != Some(artifact_hash)
            {
                errors.push(format!(
                    "bad_cache_entry: json artifact metadata hash mismatch {cache_key}"
                ));
            }
        }
        "bytes" => {
            let Some(bytes) = artifact_bytes else {
                errors.push(format!(
                    "bad_artifact_bytes: bytes artifact missing artifact_bytes {cache_key}"
                ));
                return;
            };
            let recomputed = hash_bytes(BYTES_DOMAIN, bytes);
            if recomputed != artifact_hash {
                errors.push(format!(
                    "bad_artifact_bytes: artifact bytes hash {artifact_hash} recomputes to {recomputed}"
                ));
            }
            if artifact_json.get("bytes_hash").and_then(JsonValue::as_str) != Some(artifact_hash) {
                errors.push(format!(
                    "bad_artifact_bytes: bytes artifact metadata hash mismatch {cache_key}"
                ));
            }
        }
        other => errors.push(format!(
            "bad_cache_entry: unknown artifact content kind {other:?} for {cache_key}"
        )),
    }
}

fn artifact_text(artifact_json: &JsonValue) -> Option<&str> {
    if artifact_json.get("schema").and_then(JsonValue::as_str) == Some(ARTIFACT_METADATA_SCHEMA) {
        return artifact_json.get("text").and_then(JsonValue::as_str);
    }
    artifact_json.get("text").and_then(JsonValue::as_str)
}
