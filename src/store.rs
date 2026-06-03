use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};

use crate::abi::{internal_abi_symbol, validate_export_map};
use crate::artifact::{ARTIFACT_METADATA_SCHEMA, CacheKeyInput};
use crate::backend::ArtifactKind;
use crate::model::{
    BranchState, NameBinding, ProgramRootPayload, RootSymbolPayload, normalize_root, param_names,
    preferred_names, resolve_name_in_root,
};
use crate::{BYTES_DOMAIN, CACHE_DOMAIN, MAIN_BRANCH, OBJECT_DOMAIN, SCHEMA_SQL, SCHEMA_VERSION};

pub struct CodeDb {
    pub(crate) conn: Connection,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct CacheEntry {
    pub(crate) cache_key: String,
    pub(crate) key_input: CacheKeyInput,
    pub(crate) artifact_hash: String,
    pub(crate) artifact_json: Option<JsonValue>,
    pub(crate) artifact_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(crate) enum BranchFastForwardOutcome {
    Updated {
        old: BranchState,
        new: BranchState,
    },
    StaleRoot {
        current: BranchState,
    },
    NonFastForward {
        current: BranchState,
        source: BranchState,
    },
}

impl CodeDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA_SQL)?;
        migrate_compile_cache_schema(&conn)?;
        Ok(Self { conn })
    }

    pub(crate) fn ensure_initialized(&mut self) -> Result<String> {
        self.insert_builtin_types()?;
        let root_hash = self.put_program_root(&ProgramRootPayload {
            symbols: vec![],
            names: vec![],
            param_names: vec![],
            exports: vec![],
            metadata: BTreeMap::new(),
        })?;
        self.index_root(&root_hash)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO branches (name, root_hash, history_hash) VALUES (?1, ?2, NULL)",
            params![MAIN_BRANCH, root_hash],
        )?;
        Ok(root_hash)
    }

    pub(crate) fn branch(&self, name: &str) -> Result<BranchState> {
        self.branch_opt(name)?
            .ok_or_else(|| anyhow!("branch not initialized: {name}"))
    }

    pub(crate) fn branch_opt(&self, name: &str) -> Result<Option<BranchState>> {
        self.conn
            .query_row(
                "SELECT root_hash, history_hash FROM branches WHERE name = ?1",
                params![name],
                |row| {
                    Ok(BranchState {
                        root_hash: row.get(0)?,
                        history_hash: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn create_branch_pointer(
        &mut self,
        name: &str,
        root_hash: &str,
        history_hash: Option<&str>,
    ) -> Result<BranchState> {
        validate_branch_name(name)?;
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            if self.branch_opt(name)?.is_some() {
                bail!("branch already exists: {name}");
            }
            self.load_root(root_hash)
                .with_context(|| format!("branch root is not a valid program root: {root_hash}"))?;
            if let Some(history_hash) = history_hash {
                self.ensure_history_matches_root(history_hash, root_hash)?;
            }
            self.conn.execute(
                "INSERT INTO branches (name, root_hash, history_hash, updated_at)
                 VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)",
                params![name, root_hash, history_hash],
            )?;
            Ok(BranchState {
                root_hash: root_hash.to_string(),
                history_hash: history_hash.map(str::to_string),
            })
        })();
        finish_immediate_transaction(&mut self.conn, result)
    }

    pub(crate) fn fast_forward_branch_pointer(
        &mut self,
        target: &str,
        expected_root_hash: &str,
        source: &BranchState,
    ) -> Result<BranchFastForwardOutcome> {
        validate_branch_name(target)?;
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let old = self.branch(target)?;
            if old.root_hash != expected_root_hash {
                return Ok(BranchFastForwardOutcome::StaleRoot { current: old });
            }
            if let Some(history_hash) = old.history_hash.as_deref() {
                self.ensure_history_matches_root(history_hash, &old.root_hash)?;
            }
            self.load_root(&source.root_hash).with_context(|| {
                format!(
                    "source root is not a valid program root: {}",
                    source.root_hash
                )
            })?;
            if let Some(history_hash) = source.history_hash.as_deref() {
                self.ensure_history_matches_root(history_hash, &source.root_hash)?;
            }
            if !self.branch_state_descends_from(&old, source)? {
                return Ok(BranchFastForwardOutcome::NonFastForward {
                    current: old,
                    source: source.clone(),
                });
            }
            self.conn.execute(
                "UPDATE branches
                 SET root_hash = ?2, history_hash = ?3, updated_at = CURRENT_TIMESTAMP
                 WHERE name = ?1",
                params![target, source.root_hash, source.history_hash.as_deref()],
            )?;
            Ok(BranchFastForwardOutcome::Updated {
                old,
                new: source.clone(),
            })
        })();
        finish_immediate_transaction(&mut self.conn, result)
    }

    fn branch_state_descends_from(
        &self,
        target: &BranchState,
        source: &BranchState,
    ) -> Result<bool> {
        if target.root_hash == source.root_hash && target.history_hash == source.history_hash {
            return Ok(true);
        }
        self.history_descends_from(
            source.history_hash.as_deref(),
            target.history_hash.as_deref(),
            &target.root_hash,
        )
    }

    fn history_descends_from(
        &self,
        source_history_hash: Option<&str>,
        target_history_hash: Option<&str>,
        target_root_hash: &str,
    ) -> Result<bool> {
        let mut cursor = source_history_hash.map(str::to_string);
        let mut seen = BTreeSet::new();
        while let Some(history_hash) = cursor {
            if Some(history_hash.as_str()) == target_history_hash {
                return Ok(true);
            }
            if !seen.insert(history_hash.clone()) {
                bail!("history chain contains a cycle at {history_hash}");
            }
            let (parent_history_hash, input_root_hash): (Option<String>, String) =
                self.conn.query_row(
                    "SELECT h.parent_history_hash, m.input_root_hash
                     FROM histories h
                     JOIN migrations m ON m.hash = h.migration_hash
                     WHERE h.history_hash = ?1",
                    params![history_hash],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
            if target_history_hash.is_none()
                && parent_history_hash.is_none()
                && input_root_hash == target_root_hash
            {
                return Ok(true);
            }
            cursor = parent_history_hash;
        }
        Ok(false)
    }

    pub(crate) fn delete_branch_pointer(&mut self, name: &str) -> Result<BranchState> {
        validate_branch_name(name)?;
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let state = self.branch(name)?;
            self.conn
                .execute("DELETE FROM branches WHERE name = ?1", params![name])?;
            Ok(state)
        })();
        finish_immediate_transaction(&mut self.conn, result)
    }

    pub(crate) fn cached_workspace_transaction_response(
        &self,
        request_id: &str,
        request_hash: &str,
    ) -> Result<Option<String>> {
        let cached = self
            .conn
            .query_row(
                "SELECT request_hash, response_json
                 FROM workspace_transactions
                 WHERE request_id = ?1",
                params![request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((cached_hash, response_json)) = cached else {
            return Ok(None);
        };
        if cached_hash != request_hash {
            bail!("workspace request_id {request_id:?} was already used for a different request");
        }
        Ok(Some(response_json))
    }

    pub(crate) fn record_workspace_transaction_response(
        &mut self,
        request_id: &str,
        request_hash: &str,
        method: &str,
        branch: &str,
        expected_root_hash: Option<&str>,
        response_json: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO workspace_transactions
             (request_id, request_hash, method, branch, expected_root_hash, response_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(request_id) DO UPDATE SET
                response_json = CASE
                    WHEN workspace_transactions.request_hash = excluded.request_hash
                    THEN excluded.response_json
                    ELSE workspace_transactions.response_json
                END",
            params![
                request_id,
                request_hash,
                method,
                branch,
                expected_root_hash,
                response_json,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn update_branch(
        &mut self,
        name: &str,
        root_hash: &str,
        history_hash: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO branches (name, root_hash, history_hash, updated_at)
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
             ON CONFLICT(name) DO UPDATE SET
                root_hash = excluded.root_hash,
                history_hash = excluded.history_hash,
                updated_at = CURRENT_TIMESTAMP",
            params![name, root_hash, history_hash],
        )?;
        Ok(())
    }

    fn ensure_history_matches_root(&self, history_hash: &str, root_hash: &str) -> Result<()> {
        let output_root: Option<String> = self
            .conn
            .query_row(
                "SELECT output_root_hash FROM histories WHERE history_hash = ?1",
                params![history_hash],
                |row| row.get(0),
            )
            .optional()?;
        let Some(output_root) = output_root else {
            bail!("missing history {history_hash}");
        };
        if output_root != root_hash {
            bail!("history {history_hash} outputs root {output_root}, not branch root {root_hash}");
        }
        Ok(())
    }

    pub(crate) fn put_object(&mut self, kind: &str, payload: &JsonValue) -> Result<String> {
        let canonical = canonical_json(payload);
        let hash = hash_object_canonical(kind, SCHEMA_VERSION, &canonical);
        self.conn.execute(
            "INSERT OR IGNORE INTO objects
             (hash, kind, schema_version, payload_json, payload_size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                hash,
                kind,
                SCHEMA_VERSION,
                canonical,
                canonical.len() as i64
            ],
        )?;
        self.refresh_edges(&hash, payload)?;
        Ok(hash)
    }

    pub(crate) fn refresh_edges(&mut self, parent_hash: &str, payload: &JsonValue) -> Result<()> {
        self.conn.execute(
            "DELETE FROM object_edges WHERE parent_hash = ?1",
            params![parent_hash],
        )?;
        let mut refs = Vec::new();
        extract_hash_strings(payload, &mut refs);
        let mut seen = BTreeSet::new();
        for (position, child_hash) in refs.into_iter().enumerate() {
            if !seen.insert(child_hash.clone()) {
                continue;
            }
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                params![child_hash],
                |row| row.get(0),
            )?;
            if exists && child_hash != parent_hash {
                self.conn.execute(
                    "INSERT OR IGNORE INTO object_edges
                     (parent_hash, child_hash, edge_label, edge_position)
                     VALUES (?1, ?2, 'ref', ?3)",
                    params![parent_hash, child_hash, position as i64],
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn get_payload(&self, hash: &str) -> Result<JsonValue> {
        let payload: String = self
            .conn
            .query_row(
                "SELECT payload_json FROM objects WHERE hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("missing object {hash}"))?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub(crate) fn get_kind(&self, hash: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT kind FROM objects WHERE hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("missing object {hash}"))
    }

    pub(crate) fn load_root(&self, root_hash: &str) -> Result<ProgramRootPayload> {
        let payload = self.get_payload(root_hash)?;
        let root: ProgramRootPayload = serde_json::from_value(payload)?;
        Ok(normalize_root(root))
    }

    pub(crate) fn put_program_root(&mut self, root: &ProgramRootPayload) -> Result<String> {
        let normalized = normalize_root(root.clone());
        validate_export_map(&normalized)?;
        let payload = serde_json::to_value(normalized)?;
        self.put_object("ProgramRoot", &payload)
    }

    pub(crate) fn index_root(&mut self, root_hash: &str) -> Result<()> {
        let root = self.load_root(root_hash)?;
        self.conn.execute(
            "DELETE FROM root_symbols WHERE root_hash = ?1",
            params![root_hash],
        )?;
        self.conn.execute(
            "DELETE FROM root_names WHERE root_hash = ?1",
            params![root_hash],
        )?;
        self.conn.execute(
            "DELETE FROM root_exports WHERE root_hash = ?1",
            params![root_hash],
        )?;
        self.conn.execute(
            "DELETE FROM dependencies WHERE root_hash = ?1",
            params![root_hash],
        )?;
        self.conn.execute(
            "DELETE FROM source_search WHERE root_hash = ?1",
            params![root_hash],
        )?;

        for entry in &root.symbols {
            let interface_metadata = function_interface_metadata(&entry.symbol, &entry.signature)?;
            let interface_input_hash = self.put_object("FunctionInterface", &interface_metadata)?;
            let body_hash = self.function_body_hash(&entry.definition)?;
            let direct_dependencies = self.dependencies_for_definition(&root, &entry.definition)?;
            let mut direct_dependency_interface_hashes = Vec::new();
            for dependency in &direct_dependencies {
                let Some(dependency_entry) = self.root_symbol(&root, dependency) else {
                    continue;
                };
                let dependency_interface = function_interface_metadata(
                    &dependency_entry.symbol,
                    &dependency_entry.signature,
                )?;
                direct_dependency_interface_hashes.push(hash_bytes(
                    BYTES_DOMAIN,
                    canonical_json(&dependency_interface).as_bytes(),
                ));
            }
            direct_dependency_interface_hashes.sort();
            direct_dependency_interface_hashes.dedup();
            self.conn.execute(
                "INSERT OR REPLACE INTO root_symbols
                 (root_hash, symbol_hash, definition_hash, signature_hash)
                 VALUES (?1, ?2, ?3, ?4)",
                params![root_hash, entry.symbol, entry.definition, entry.signature],
            )?;
            self.write_cache_json(
                &interface_input_hash,
                "typechecker",
                "interface",
                ArtifactKind::InterfaceHash,
                &interface_metadata,
            )?;
            let implementation_metadata = json!({
                "symbol_hash": entry.symbol,
                "definition_hash": entry.definition,
                "function_sig_hash": entry.signature,
                "typed_body_expr_hash": body_hash,
                "internal_abi_symbol": internal_abi_symbol(&entry.symbol)?,
                "direct_dependency_symbols": direct_dependencies.iter().cloned().collect::<Vec<_>>(),
                "direct_dependency_interface_hashes": direct_dependency_interface_hashes,
                "semantic_lowering_version": crate::lowering::LOWERED_IR_SCHEMA,
            });
            let implementation_key = CacheKeyInput::new(
                ArtifactKind::ImplementationHash,
                &entry.definition,
                "lowering",
                "implementation",
            )
            .with_dependency_interface_hashes(direct_dependency_interface_hashes.clone());
            self.write_cache_json_for_key(implementation_key, &implementation_metadata)?;
        }

        for binding in &root.names {
            self.conn.execute(
                "INSERT OR REPLACE INTO root_names
                 (root_hash, module_name, display_name, symbol_hash, is_preferred)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    root_hash,
                    binding.module,
                    binding.display_name,
                    binding.symbol,
                    if binding.is_preferred { 1 } else { 0 }
                ],
            )?;
        }

        for binding in &root.exports {
            self.conn.execute(
                "INSERT OR REPLACE INTO root_exports
                 (root_hash, exported_name, symbol_hash)
                 VALUES (?1, ?2, ?3)",
                params![root_hash, binding.exported_name, binding.symbol],
            )?;
        }

        for entry in &root.symbols {
            let deps = self.dependencies_for_definition(&root, &entry.definition)?;
            self.write_cache_json(
                &entry.definition,
                "analysis",
                "dependencies",
                ArtifactKind::FunctionDependencySet,
                &json!({ "dependencies": deps.iter().cloned().collect::<Vec<_>>() }),
            )?;
            for dep in deps {
                self.conn.execute(
                    "INSERT OR IGNORE INTO dependencies
                     (root_hash, from_symbol_hash, to_symbol_hash)
                     VALUES (?1, ?2, ?3)",
                    params![root_hash, entry.symbol, dep],
                )?;
            }
        }

        for binding in preferred_names(&root) {
            let symbol = binding.symbol.clone();
            if let Some(entry) = self.root_symbol(&root, &symbol) {
                let body = self.function_body_hash(&entry.definition)?;
                let source = format!(
                    "fn {}{} = {}",
                    binding.display_name,
                    self.signature_source(&entry.signature, &param_names(&root, &symbol))?,
                    self.expr_to_source(&body, &root, &param_names(&root, &symbol), 0)?
                );
                self.conn.execute(
                    "INSERT INTO source_search (root_hash, symbol_hash, rendered_source)
                     VALUES (?1, ?2, ?3)",
                    params![root_hash, symbol, source],
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn dependencies_for_symbol(
        &self,
        root_hash: &str,
        symbol: &str,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT to_symbol_hash FROM dependencies
             WHERE root_hash = ?1 AND from_symbol_hash = ?2 ORDER BY to_symbol_hash",
        )?;
        Ok(stmt
            .query_map(params![root_hash, symbol], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn direct_dependents_for_symbol(
        &self,
        root_hash: &str,
        symbol: &str,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_symbol_hash FROM dependencies
             WHERE root_hash = ?1 AND to_symbol_hash = ?2 ORDER BY from_symbol_hash",
        )?;
        Ok(stmt
            .query_map(params![root_hash, symbol], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn transitive_dependents_for_symbol(
        &self,
        root_hash: &str,
        symbol: &str,
    ) -> Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        let mut frontier = self.direct_dependents_for_symbol(root_hash, symbol)?;
        frontier.sort();
        while let Some(dependent) = frontier.pop() {
            if !seen.insert(dependent.clone()) {
                continue;
            }
            for next in self.direct_dependents_for_symbol(root_hash, &dependent)? {
                if !seen.contains(&next) {
                    frontier.push(next);
                }
            }
            frontier.sort();
        }
        Ok(seen.into_iter().collect())
    }

    pub(crate) fn reverse_dependencies_for_root(
        &self,
        root: &ProgramRootPayload,
        symbol: &str,
    ) -> Result<Vec<String>> {
        let mut callers = Vec::new();
        for entry in &root.symbols {
            let deps = self.dependencies_for_definition(root, &entry.definition)?;
            if deps.contains(symbol) {
                callers.push(entry.symbol.clone());
            }
        }
        callers.sort();
        Ok(callers)
    }

    pub(crate) fn resolve_name(&self, root_hash: &str, module: &str, name: &str) -> Result<String> {
        let root = self.load_root(root_hash)?;
        resolve_name_in_root(&root, module, name)
            .ok_or_else(|| anyhow!("unknown name {module}.{name}"))
    }

    pub(crate) fn resolve_symbol_or_name(
        &self,
        root_hash: &str,
        symbol_or_name: &str,
    ) -> Result<String> {
        if symbol_or_name.starts_with("sha256:") {
            return Ok(symbol_or_name.to_string());
        }
        self.resolve_name(root_hash, "main", symbol_or_name)
    }

    pub(crate) fn root_symbol<'a>(
        &self,
        root: &'a ProgramRootPayload,
        symbol: &str,
    ) -> Option<&'a RootSymbolPayload> {
        root.symbols.iter().find(|entry| entry.symbol == symbol)
    }

    pub(crate) fn preferred_binding<'a>(
        &self,
        root: &'a ProgramRootPayload,
        symbol: &str,
    ) -> Option<&'a NameBinding> {
        root.names
            .iter()
            .find(|binding| binding.symbol == symbol && binding.is_preferred)
            .or_else(|| root.names.iter().find(|binding| binding.symbol == symbol))
    }

    pub(crate) fn symbol_display(&self, root: &ProgramRootPayload, symbol: &str) -> Result<String> {
        self.preferred_binding(root, symbol)
            .map(|binding| binding.display_name.clone())
            .ok_or_else(|| anyhow!("symbol has no display name {symbol}"))
    }

    pub(crate) fn write_cache_text(
        &mut self,
        input_hash: &str,
        backend: &str,
        target: &str,
        artifact_kind: ArtifactKind,
        text: &str,
    ) -> Result<()> {
        let key_input = CacheKeyInput::new(artifact_kind, input_hash, backend, target);
        let artifact_hash = hash_bytes(BYTES_DOMAIN, text.as_bytes());
        let artifact_json = text_artifact_metadata(&key_input, text, &artifact_hash);
        self.write_cache_entry(&key_input, &artifact_hash, Some(&artifact_json), None)
    }

    pub(crate) fn write_cache_json(
        &mut self,
        input_hash: &str,
        backend: &str,
        target: &str,
        artifact_kind: ArtifactKind,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let key_input = CacheKeyInput::new(artifact_kind, input_hash, backend, target);
        let artifact_hash = hash_bytes(BYTES_DOMAIN, canonical_json(artifact_json).as_bytes());
        let artifact_json = json_artifact_metadata(&key_input, artifact_json);
        self.write_cache_entry(&key_input, &artifact_hash, Some(&artifact_json), None)
    }

    pub(crate) fn write_cache_json_for_key(
        &mut self,
        key_input: CacheKeyInput,
        artifact_json: &JsonValue,
    ) -> Result<String> {
        let key_input = key_input.normalized();
        let artifact_hash = hash_bytes(BYTES_DOMAIN, canonical_json(artifact_json).as_bytes());
        let artifact_json = json_artifact_metadata(&key_input, artifact_json);
        self.write_cache_entry(&key_input, &artifact_hash, Some(&artifact_json), None)?;
        Ok(artifact_hash)
    }

    #[allow(dead_code, clippy::too_many_arguments)]
    pub(crate) fn write_cache(
        &mut self,
        input_hash: &str,
        backend: &str,
        target: &str,
        artifact_kind: ArtifactKind,
        artifact_hash: &str,
        artifact_json: Option<&JsonValue>,
        artifact_bytes: Option<&[u8]>,
    ) -> Result<()> {
        let key_input = CacheKeyInput::new(artifact_kind, input_hash, backend, target);
        self.write_cache_entry(&key_input, artifact_hash, artifact_json, artifact_bytes)
    }

    #[allow(dead_code)]
    pub(crate) fn write_cache_bytes(
        &mut self,
        key_input: CacheKeyInput,
        metadata: &JsonValue,
        artifact_bytes: &[u8],
    ) -> Result<()> {
        let key_input = key_input.normalized();
        let artifact_hash = hash_bytes(BYTES_DOMAIN, artifact_bytes);
        let artifact_json = bytes_artifact_metadata(&key_input, metadata, &artifact_hash);
        self.write_cache_entry(
            &key_input,
            &artifact_hash,
            Some(&artifact_json),
            Some(artifact_bytes),
        )
    }

    pub(crate) fn write_cache_entry(
        &mut self,
        key_input: &CacheKeyInput,
        artifact_hash: &str,
        artifact_json: Option<&JsonValue>,
        artifact_bytes: Option<&[u8]>,
    ) -> Result<()> {
        let key_input = key_input.clone().normalized();
        key_input.validate()?;
        if key_input.artifact_kind.is_compiler_artifact() && key_input.backend_id == "projection" {
            bail!("compiler artifacts must be emitted by a compiler backend");
        }
        if key_input.artifact_kind.requires_artifact_bytes() && artifact_bytes.is_none() {
            bail!("native object and executable artifacts must use artifact_bytes");
        }
        let cache_key_json = cache_key_json(&key_input)?;
        let cache_key = hash_bytes(CACHE_DOMAIN, cache_key_json.as_bytes());
        let artifact_json_canonical = artifact_json.map(canonical_json);
        self.conn.execute(
            "INSERT OR IGNORE INTO compile_cache
             (cache_key, cache_key_json, input_hash, backend, target, compiler_version, artifact_kind,
              artifact_hash, artifact_json, artifact_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &cache_key,
                &cache_key_json,
                key_input.input_hash,
                key_input.backend_id,
                key_input.target_triple,
                key_input.compiler_version,
                key_input.artifact_kind.as_str(),
                artifact_hash,
                artifact_json_canonical.as_deref(),
                artifact_bytes,
            ],
        )?;
        if self.conn.changes() == 1 {
            return Ok(());
        }

        let (
            existing_key_json,
            existing_artifact_hash,
            existing_artifact_json,
            existing_artifact_bytes,
        ) = self
            .conn
            .query_row(
                "SELECT cache_key_json, artifact_hash, artifact_json, artifact_bytes
                 FROM compile_cache WHERE cache_key = ?1",
                params![&cache_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("cache key {cache_key} was not inserted and cannot be read"))?;
        if existing_key_json != cache_key_json {
            bail!("cache conflict: cache key {cache_key} has mismatched key input JSON");
        }
        if existing_artifact_hash != artifact_hash {
            if existing_artifact_bytes.is_none() && artifact_bytes.is_none() {
                self.conn.execute(
                    "UPDATE compile_cache
                     SET artifact_hash = ?2,
                         artifact_json = ?3,
                         artifact_bytes = NULL
                     WHERE cache_key = ?1",
                    params![
                        &cache_key,
                        artifact_hash,
                        artifact_json_canonical.as_deref()
                    ],
                )?;
                return Ok(());
            }
            bail!(
                "cache conflict: cache key {cache_key} already stores artifact {existing_artifact_hash}, not {artifact_hash}"
            );
        }
        if existing_artifact_bytes.as_deref() != artifact_bytes {
            bail!("cache conflict: cache key {cache_key} has different bytes for {artifact_hash}");
        }
        if existing_artifact_json != artifact_json_canonical {
            self.conn.execute(
                "UPDATE compile_cache
                 SET artifact_json = ?2,
                     artifact_bytes = ?3
                 WHERE cache_key = ?1",
                params![
                    &cache_key,
                    artifact_json_canonical.as_deref(),
                    artifact_bytes
                ],
            )?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn lookup_cache(&self, key_input: &CacheKeyInput) -> Result<Option<CacheEntry>> {
        let key_input = key_input.clone().normalized();
        key_input.validate()?;
        let cache_key = cache_key_for_input(&key_input)?;
        let Some((cache_key_json, artifact_hash, artifact_json, artifact_bytes)) = self
            .conn
            .query_row(
                "SELECT cache_key_json, artifact_hash, artifact_json, artifact_bytes
                 FROM compile_cache WHERE cache_key = ?1",
                params![cache_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let stored_key_input = serde_json::from_str::<CacheKeyInput>(&cache_key_json)?.normalized();
        Ok(Some(CacheEntry {
            cache_key,
            key_input: stored_key_input,
            artifact_hash,
            artifact_json: artifact_json
                .map(|json| serde_json::from_str::<JsonValue>(&json))
                .transpose()?,
            artifact_bytes,
        }))
    }
}

pub(crate) fn cache_key_for_input(key_input: &CacheKeyInput) -> Result<String> {
    let key_input = key_input.clone().normalized();
    key_input.validate()?;
    Ok(hash_bytes(
        CACHE_DOMAIN,
        cache_key_json(&key_input)?.as_bytes(),
    ))
}

fn validate_branch_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("branch name must not be empty");
    }
    if name.trim() != name || name.chars().any(char::is_whitespace) {
        bail!("branch name must not contain whitespace");
    }
    if name.chars().any(char::is_control) {
        bail!("branch name must not contain control characters");
    }
    Ok(())
}

fn finish_immediate_transaction<T>(conn: &mut Connection, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            conn.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(err) => {
            if let Err(rollback_err) = conn.execute_batch("ROLLBACK") {
                return Err(err).context(format!("rollback failed: {rollback_err}"));
            }
            Err(err)
        }
    }
}

pub(crate) fn function_interface_metadata(symbol: &str, signature: &str) -> Result<JsonValue> {
    Ok(json!({
        "symbol_hash": symbol,
        "signature_hash": signature,
        "internal_abi_symbol": internal_abi_symbol(symbol)?,
    }))
}

fn migrate_compile_cache_schema(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(compile_cache)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    drop(stmt);

    if !columns.contains("cache_key_json") {
        conn.execute(
            "ALTER TABLE compile_cache ADD COLUMN cache_key_json TEXT",
            [],
        )?;
    }
    Ok(())
}

pub(crate) fn cache_key_json(key_input: &CacheKeyInput) -> Result<String> {
    let key_input = key_input.clone().normalized();
    key_input.validate()?;
    Ok(canonical_json(&serde_json::to_value(key_input)?))
}

fn text_artifact_metadata(key_input: &CacheKeyInput, text: &str, text_hash: &str) -> JsonValue {
    json!({
        "schema": ARTIFACT_METADATA_SCHEMA,
        "artifact_kind": key_input.artifact_kind.as_str(),
        "input_hash": &key_input.input_hash,
        "backend_id": &key_input.backend_id,
        "target_triple": &key_input.target_triple,
        "content_kind": "text",
        "text": text,
        "text_hash": text_hash,
    })
}

fn json_artifact_metadata(key_input: &CacheKeyInput, metadata: &JsonValue) -> JsonValue {
    json!({
        "schema": ARTIFACT_METADATA_SCHEMA,
        "artifact_kind": key_input.artifact_kind.as_str(),
        "input_hash": &key_input.input_hash,
        "backend_id": &key_input.backend_id,
        "target_triple": &key_input.target_triple,
        "content_kind": "json",
        "metadata": metadata,
        "metadata_hash": hash_bytes(BYTES_DOMAIN, canonical_json(metadata).as_bytes()),
    })
}

fn bytes_artifact_metadata(
    key_input: &CacheKeyInput,
    metadata: &JsonValue,
    bytes_hash: &str,
) -> JsonValue {
    json!({
        "schema": ARTIFACT_METADATA_SCHEMA,
        "artifact_kind": key_input.artifact_kind.as_str(),
        "input_hash": &key_input.input_hash,
        "backend_id": &key_input.backend_id,
        "target_triple": &key_input.target_triple,
        "content_kind": "bytes",
        "metadata": metadata,
        "bytes_hash": bytes_hash,
    })
}

pub(crate) fn hash_object_canonical(
    kind: &str,
    schema_version: i64,
    canonical_payload: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(OBJECT_DOMAIN);
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(schema_version.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_payload.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

pub(crate) fn hash_bytes(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

pub(crate) fn canonical_json(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => serde_json::to_string(value).expect("string serialization"),
        JsonValue::Array(values) => {
            let inner = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        JsonValue::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key serialization"),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
    }
}

pub(crate) fn extract_hash_strings(value: &JsonValue, out: &mut Vec<String>) {
    match value {
        JsonValue::String(value) => {
            if value.starts_with("sha256:") {
                out.push(value.clone());
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                extract_hash_strings(value, out);
            }
        }
        JsonValue::Object(map) => {
            for value in map.values() {
                extract_hash_strings(value, out);
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn object_cache_key_includes_target_and_lookup_round_trips_bytes() {
        let temp = tempdir().unwrap();
        let mut db = CodeDb::open(temp.path().join("cache.sqlite")).unwrap();
        db.init().unwrap();
        let symbol_hash = db.put_symbol_birth(None, "demo").unwrap();
        let i64_type = db.resolve_type("i64").unwrap();
        let signature_hash = db.put_signature(&[], &i64_type).unwrap();
        let body_hash = db
            .put_object(
                "Expression",
                &json!({
                    "expr_kind": "literal_i64",
                    "value": "1",
                    "type": i64_type,
                }),
            )
            .unwrap();
        let input_hash = db
            .put_function_def(&symbol_hash, &signature_hash, &body_hash)
            .unwrap();
        let internal_symbol = internal_abi_symbol(&symbol_hash).unwrap();
        let dep_interface = hash_bytes(BYTES_DOMAIN, b"callee-interface");
        let linux_key = CacheKeyInput::new(
            ArtifactKind::ObjectFile,
            &input_hash,
            "native-elf-v0",
            "x86_64-unknown-linux-gnu",
        )
        .with_dependency_interface_hashes(vec![dep_interface.clone()]);
        let darwin_key = CacheKeyInput::new(
            ArtifactKind::ObjectFile,
            &input_hash,
            "native-elf-v0",
            "aarch64-apple-darwin",
        )
        .with_dependency_interface_hashes(vec![dep_interface.clone()]);

        assert_ne!(
            cache_key_for_input(&linux_key).unwrap(),
            cache_key_for_input(&darwin_key).unwrap()
        );

        db.write_cache_bytes(
            linux_key.clone(),
            &json!({
                "schema": "codedb/native-object/v1",
                "backend_id": "native-elf-v0",
                "object_format": "elf64-x86-64-relocatable",
                "target_triple": "x86_64-unknown-linux-gnu",
                "symbol_hash": symbol_hash,
                "function_def_hash": input_hash,
                "function_sig_hash": signature_hash,
                "typed_body_expr_hash": body_hash,
                "lowered_ir_schema": "codedb/lowered-function-ir/v1",
                "defined_symbols": [internal_symbol],
                "called_symbols": [],
                "relocations": [],
                "dependency_interface_hashes": [dep_interface],
                "dependency_closure": [],
            }),
            b"\x7fELF",
        )
        .unwrap();

        let entry = db.lookup_cache(&linux_key).unwrap().unwrap();
        assert_eq!(entry.artifact_bytes.as_deref(), Some(&b"\x7fELF"[..]));
        assert!(db.lookup_cache(&darwin_key).unwrap().is_none());
        db.verify().unwrap();
    }

    #[test]
    fn cache_write_rejects_same_key_with_different_artifact_hash() {
        let temp = tempdir().unwrap();
        let mut db = CodeDb::open(temp.path().join("cache-conflict.sqlite")).unwrap();
        db.init().unwrap();
        let input_hash = db
            .put_object("CacheInput", &json!({"schema": "test/input"}))
            .unwrap();
        let key = CacheKeyInput::new(
            ArtifactKind::ObjectFile,
            &input_hash,
            "native-elf-v0",
            "x86_64-unknown-linux-gnu",
        );

        db.write_cache_bytes(key.clone(), &json!({"version": 1}), b"one")
            .unwrap();
        let err = db
            .write_cache_bytes(key.clone(), &json!({"version": 2}), b"two")
            .unwrap_err();
        assert!(format!("{err:#}").contains("cache conflict"));

        let entry = db.lookup_cache(&key).unwrap().unwrap();
        assert_eq!(entry.artifact_bytes.as_deref(), Some(&b"one"[..]));
    }
}
