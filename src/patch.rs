use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::MAIN_BRANCH;
use crate::expr::{RawCaseArm, RawExpr, RawRecordField};
use crate::migrations::{MigrationOutcome, MigrationReport, Operation};
use crate::model::{ProgramRootPayload, resolve_name_in_root};
use crate::store::{CodeDb, canonical_json, hash_bytes};
use crate::types::ParamSpec;

const SEMANTIC_PATCH_SCHEMA: &str = "codedb/semantic-patch/v1";
const SEMANTIC_PATCH_PREVIEW_SCHEMA: &str = "codedb/semantic-patch-preview/v1";
const SEMANTIC_PATCH_APPLY_RESULT_SCHEMA: &str = "codedb/semantic-patch-apply-result/v1";
const SEMANTIC_PATCH_PROVENANCE_SCHEMA: &str = "codedb/semantic-patch-provenance/v1";
const SEMANTIC_PATCH_HASH_DOMAIN: &[u8] = b"codedb/semantic-patch/v1\0";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SemanticPatch {
    #[serde(default)]
    schema: Option<String>,
    #[serde(default = "default_branch")]
    branch: String,
    #[serde(
        default,
        alias = "expected_root",
        alias = "expect_root",
        alias = "expect_root_hash"
    )]
    expected_root_hash: Option<String>,
    #[serde(default, rename = "match")]
    match_pattern: Option<PatchMatch>,
    #[serde(default)]
    replace: Option<PatchReplace>,
    #[serde(default)]
    agent: Option<JsonValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PatchMatch {
    Symbol {
        #[serde(default)]
        module: Option<String>,
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    FunctionDefinition {
        #[serde(default)]
        module: Option<String>,
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        definition_hash: Option<String>,
    },
    Expr {
        #[serde(default)]
        expr_hash: Option<String>,
        #[serde(default)]
        expr_kind: Option<String>,
        #[serde(default)]
        within_symbol: Option<String>,
        #[serde(default, alias = "within_function")]
        within_name: Option<String>,
        #[serde(default)]
        within_module: Option<String>,
    },
    LiteralI64 {
        value: String,
        #[serde(default)]
        within_symbol: Option<String>,
        #[serde(default, alias = "within_function")]
        within_name: Option<String>,
        #[serde(default)]
        within_module: Option<String>,
    },
    LiteralBool {
        value: bool,
        #[serde(default)]
        within_symbol: Option<String>,
        #[serde(default, alias = "within_function")]
        within_name: Option<String>,
        #[serde(default)]
        within_module: Option<String>,
    },
    Call {
        #[serde(default)]
        target_symbol: Option<String>,
        #[serde(default, alias = "target", alias = "name")]
        target_name: Option<String>,
        #[serde(default)]
        target_module: Option<String>,
        #[serde(default)]
        within_symbol: Option<String>,
        #[serde(default, alias = "within_function")]
        within_name: Option<String>,
        #[serde(default)]
        within_module: Option<String>,
    },
    Type {
        #[serde(default)]
        type_hash: Option<String>,
        #[serde(default, alias = "type_name")]
        name: Option<String>,
    },
    Export {
        #[serde(default)]
        exported_name: Option<String>,
        #[serde(default)]
        module: Option<String>,
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PatchReplace {
    LiteralI64 {
        value: String,
    },
    LiteralBool {
        value: bool,
    },
    Unit,
    Call {
        #[serde(default)]
        target_symbol: Option<String>,
        #[serde(default, alias = "target", alias = "name")]
        target_name: Option<String>,
        #[serde(default)]
        target_module: Option<String>,
        #[serde(default)]
        args: Option<JsonValue>,
    },
    RenameSymbol {
        new_name: String,
    },
    ExtractFunction {
        name: String,
        #[serde(default)]
        birth_seed: Option<String>,
        #[serde(default)]
        params: Vec<ParamSpec>,
        #[serde(default)]
        return_type: Option<String>,
        #[serde(default)]
        args: Vec<RawExpr>,
    },
    InlineFunction,
    AddParameter {
        name: String,
        #[serde(rename = "type")]
        ty: String,
        #[serde(default, alias = "default_arg")]
        default: Option<RawExpr>,
    },
    RemoveUnusedSymbol,
    SetExport {
        exported_name: String,
    },
    RemoveExport {
        exported_name: String,
    },
}

#[derive(Debug, Clone, Default)]
struct MatchSet {
    symbols: Vec<SymbolMatch>,
    expressions: Vec<ExpressionMatch>,
    types: Vec<TypeMatch>,
    exports: Vec<ExportMatch>,
}

impl MatchSet {
    fn sort_dedup(&mut self) {
        self.symbols
            .sort_by(|a, b| a.symbol_hash.cmp(&b.symbol_hash));
        self.symbols.dedup_by(|a, b| a.symbol_hash == b.symbol_hash);

        self.expressions.sort_by(|a, b| {
            (&a.symbol_hash, &a.expr_hash, &a.expr_kind).cmp(&(
                &b.symbol_hash,
                &b.expr_hash,
                &b.expr_kind,
            ))
        });
        self.expressions
            .dedup_by(|a, b| a.symbol_hash == b.symbol_hash && a.expr_hash == b.expr_hash);

        self.types.sort_by(|a, b| {
            (&a.type_hash, &a.owner_kind, &a.owner_hash).cmp(&(
                &b.type_hash,
                &b.owner_kind,
                &b.owner_hash,
            ))
        });
        self.types.dedup_by(|a, b| {
            a.type_hash == b.type_hash
                && a.owner_kind == b.owner_kind
                && a.owner_hash == b.owner_hash
        });

        self.exports.sort_by(|a, b| {
            (&a.exported_name, &a.symbol_hash).cmp(&(&b.exported_name, &b.symbol_hash))
        });
        self.exports
            .dedup_by(|a, b| a.exported_name == b.exported_name && a.symbol_hash == b.symbol_hash);
    }

    fn match_count(&self) -> usize {
        self.symbols.len() + self.expressions.len() + self.types.len() + self.exports.len()
    }
}

#[derive(Debug, Clone)]
struct SymbolMatch {
    module: String,
    name: String,
    symbol_hash: String,
    signature_hash: String,
    definition_hash: String,
}

#[derive(Debug, Clone)]
struct ExpressionMatch {
    module: String,
    symbol_name: String,
    symbol_hash: String,
    definition_hash: String,
    expr_hash: String,
    expr_kind: String,
    type_hash: Option<String>,
    literal_value: Option<JsonValue>,
    target_symbol_hash: Option<String>,
    target_name: Option<String>,
}

#[derive(Debug, Clone)]
struct TypeMatch {
    type_hash: String,
    type_name: String,
    owner_kind: String,
    owner_hash: String,
    symbol_hash: Option<String>,
    symbol_name: Option<String>,
}

#[derive(Debug, Clone)]
struct ExportMatch {
    exported_name: String,
    symbol_hash: String,
    symbol_name: String,
}

#[derive(Debug, Clone)]
enum ExprReplacement {
    LiteralI64(String),
    LiteralBool(bool),
    Unit,
    CallTarget {
        target_name: String,
    },
    NewCall {
        target_name: String,
        args: Vec<RawExpr>,
    },
}

impl CodeDb {
    pub fn preview_semantic_patch_json_file(&mut self, path: &Path) -> Result<String> {
        self.ensure_initialized()?;
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        self.preview_semantic_patch_json_str(&text)
            .with_context(|| format!("failed to preview {}", path.display()))
    }

    pub fn preview_semantic_patch_json_str(&mut self, text: &str) -> Result<String> {
        self.ensure_initialized()?;
        let patch = parse_semantic_patch(text)?;
        self.preview_semantic_patch(patch)
    }

    pub fn apply_semantic_patch_json_file(&mut self, path: &Path) -> Result<String> {
        self.ensure_initialized()?;
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        self.apply_semantic_patch_json_str(&text)
            .with_context(|| format!("failed to apply {}", path.display()))
    }

    pub fn apply_semantic_patch_json_str(&mut self, text: &str) -> Result<String> {
        self.ensure_initialized()?;
        let patch = parse_semantic_patch(text)?;
        self.apply_semantic_patch(patch)
    }

    fn preview_semantic_patch(&mut self, patch: SemanticPatch) -> Result<String> {
        if let Some(schema) = &patch.schema
            && schema != SEMANTIC_PATCH_SCHEMA
        {
            bail!("unsupported semantic patch schema {schema:?}; expected {SEMANTIC_PATCH_SCHEMA}");
        }
        let branch = self.branch(&patch.branch)?;
        let root_hash = patch
            .expected_root_hash
            .clone()
            .unwrap_or_else(|| branch.root_hash.clone());
        let root = self.load_root(&root_hash)?;
        let mut matches = self.match_semantic_patch(&root, &patch)?;
        matches.sort_dedup();
        let planned_operations =
            self.plan_semantic_patch_operations(&root_hash, &root, &patch, &matches)?;
        let planned_operations_json = planned_operations
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let apply_preview = if planned_operations.is_empty() {
            None
        } else {
            let apply_document = json!({
                "schema": "codedb/apply/v1",
                "branch": patch.branch,
                "expect_root_hash": root_hash,
                "operations": planned_operations_json,
            });
            let preview = self.preview_apply_json_str(&canonical_json(&apply_document))?;
            Some(serde_json::from_str::<JsonValue>(preview.trim_end())?)
        };
        let status = semantic_patch_preview_status(&matches, apply_preview.as_ref());
        let typecheck = semantic_patch_typecheck_status(apply_preview.as_ref());
        let build_impacts = semantic_patch_build_impacts(apply_preview.as_ref());
        let build_impact = if build_impacts.len() == 1 {
            build_impacts.first().cloned().unwrap_or(JsonValue::Null)
        } else if build_impacts.is_empty() {
            JsonValue::Null
        } else {
            json!({
                "kind": "multiple",
                "operation_impacts": build_impacts,
            })
        };
        let conflicts = semantic_patch_conflicts(apply_preview.as_ref());
        let diagnostics = semantic_patch_diagnostics(&status, &matches, apply_preview.as_ref());
        let payload = json!({
            "schema": SEMANTIC_PATCH_PREVIEW_SCHEMA,
            "status": status,
            "branch": patch.branch,
            "root_hash": root_hash,
            "current_root_hash": branch.root_hash,
            "current_history_hash": branch.history_hash,
            "expected_root_hash": patch.expected_root_hash,
            "match_count": matches.match_count(),
            "matched_symbols": symbol_matches_json(&matches.symbols),
            "matched_expressions": expression_matches_json(&matches.expressions),
            "matched_types": type_matches_json(&matches.types),
            "matched_exports": export_matches_json(&matches.exports),
            "planned_operation_count": planned_operations.len(),
            "planned_operations": planned_operations_json,
            "typecheck": typecheck,
            "build_impact": build_impact,
            "build_impacts": build_impacts,
            "conflicts": conflicts,
            "diagnostics": diagnostics,
            "apply_preview": apply_preview,
        });
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    fn apply_semantic_patch(&mut self, patch: SemanticPatch) -> Result<String> {
        if let Some(schema) = &patch.schema
            && schema != SEMANTIC_PATCH_SCHEMA
        {
            bail!("unsupported semantic patch schema {schema:?}; expected {SEMANTIC_PATCH_SCHEMA}");
        }
        let expected_root_hash = patch.expected_root_hash.clone().ok_or_else(|| {
            anyhow!("semantic patch apply requires expected_root or expected_root_hash")
        })?;
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.apply_semantic_patch_in_tx(patch, &expected_root_hash);
        match result {
            Ok((payload, should_commit)) => {
                if should_commit {
                    self.conn.execute_batch("COMMIT")?;
                } else {
                    self.conn.execute_batch("ROLLBACK")?;
                }
                Ok(payload)
            }
            Err(err) => {
                if let Err(rollback_err) = self.conn.execute_batch("ROLLBACK") {
                    return Err(err).context(format!("rollback failed: {rollback_err}"));
                }
                Err(err)
            }
        }
    }

    fn apply_semantic_patch_in_tx(
        &mut self,
        patch: SemanticPatch,
        expected_root_hash: &str,
    ) -> Result<(String, bool)> {
        let branch_before = self.branch(&patch.branch)?;
        let root = self.load_root(expected_root_hash)?;
        let mut matches = self.match_semantic_patch(&root, &patch)?;
        matches.sort_dedup();
        let planned_operations =
            self.plan_semantic_patch_operations(expected_root_hash, &root, &patch, &matches)?;
        let planned_operations_json = planned_operations
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let (apply_result, should_commit) = if planned_operations.is_empty() {
            if branch_before.root_hash == expected_root_hash {
                (None, false)
            } else {
                (
                    Some(stale_root_patch_apply_result(
                        &patch.branch,
                        expected_root_hash,
                        &branch_before.root_hash,
                        branch_before.history_hash.as_deref(),
                    )),
                    false,
                )
            }
        } else if branch_before.root_hash != expected_root_hash
            && self.recorded_operation_sequence_outputs_root(
                expected_root_hash,
                &branch_before.root_hash,
                &planned_operations,
            )?
        {
            (
                Some(self.already_applied_patch_apply_result(
                    &patch.branch,
                    &branch_before.root_hash,
                    branch_before.history_hash.as_deref(),
                    &planned_operations,
                )?),
                false,
            )
        } else {
            let apply_document = json!({
                "schema": "codedb/apply/v1",
                "branch": &patch.branch,
                "expect_root_hash": expected_root_hash,
                "agent": semantic_patch_agent_metadata(
                    &patch,
                    expected_root_hash,
                    &matches,
                    &planned_operations_json,
                )?,
                "operations": planned_operations_json,
            });
            let (applied, should_commit) =
                self.apply_json_str_in_current_transaction(&canonical_json(&apply_document))?;
            (
                Some(serde_json::from_str::<JsonValue>(applied.trim_end())?),
                should_commit,
            )
        };
        let branch_after = self.branch(&patch.branch)?;
        let status =
            semantic_patch_apply_status(&matches, apply_result.as_ref(), patch.replace.is_some());
        let typecheck = semantic_patch_typecheck_status(apply_result.as_ref());
        let build_impacts = semantic_patch_build_impacts(apply_result.as_ref());
        let build_impact = if build_impacts.len() == 1 {
            build_impacts.first().cloned().unwrap_or(JsonValue::Null)
        } else if build_impacts.is_empty() {
            JsonValue::Null
        } else {
            json!({
                "kind": "multiple",
                "operation_impacts": build_impacts,
            })
        };
        let conflicts = semantic_patch_conflicts(apply_result.as_ref());
        let diagnostics = semantic_patch_diagnostics(&status, &matches, apply_result.as_ref());
        let payload = json!({
            "schema": SEMANTIC_PATCH_APPLY_RESULT_SCHEMA,
            "status": status,
            "branch": &patch.branch,
            "expected_root_hash": expected_root_hash,
            "old_root_hash": apply_result
                .as_ref()
                .and_then(|result| result.get("old_root_hash"))
                .and_then(JsonValue::as_str)
                .unwrap_or(&branch_before.root_hash),
            "new_root_hash": apply_result
                .as_ref()
                .and_then(|result| result.get("new_root_hash"))
                .and_then(JsonValue::as_str)
                .unwrap_or(&branch_after.root_hash),
            "old_history_hash": apply_result
                .as_ref()
                .and_then(|result| result.get("old_history_hash"))
                .cloned()
                .unwrap_or_else(|| json!(&branch_before.history_hash)),
            "new_history_hash": apply_result
                .as_ref()
                .and_then(|result| result.get("new_history_hash"))
                .cloned()
                .unwrap_or_else(|| json!(&branch_after.history_hash)),
            "current_root_hash": &branch_after.root_hash,
            "current_history_hash": &branch_after.history_hash,
            "committed": apply_result
                .as_ref()
                .and_then(|result| result.get("committed"))
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
            "patch_hash": semantic_patch_hash(&patch)?,
            "match_count": matches.match_count(),
            "matched_symbols": symbol_matches_json(&matches.symbols),
            "matched_expressions": expression_matches_json(&matches.expressions),
            "matched_types": type_matches_json(&matches.types),
            "matched_exports": export_matches_json(&matches.exports),
            "planned_operation_count": planned_operations.len(),
            "planned_operations": planned_operations_json,
            "semantic_summary": semantic_patch_semantic_summary(
                &matches,
                apply_result.as_ref(),
            ),
            "typecheck": typecheck,
            "build_impact": build_impact,
            "build_impacts": build_impacts,
            "conflicts": conflicts,
            "diagnostics": diagnostics,
            "apply_result": apply_result,
        });
        Ok((format!("{}\n", canonical_json(&payload)), should_commit))
    }

    fn match_semantic_patch(
        &self,
        root: &ProgramRootPayload,
        patch: &SemanticPatch,
    ) -> Result<MatchSet> {
        let Some(pattern) = &patch.match_pattern else {
            bail!("semantic patch requires a match object");
        };
        let mut matches = MatchSet::default();
        match pattern {
            PatchMatch::Symbol {
                module,
                symbol,
                name,
            } => {
                for symbol_match in self.match_symbols(root, module.as_deref(), symbol, name)? {
                    matches.symbols.push(symbol_match);
                }
            }
            PatchMatch::FunctionDefinition {
                module,
                symbol,
                name,
                definition_hash,
            } => {
                for symbol_match in self.match_symbols(root, module.as_deref(), symbol, name)? {
                    if definition_hash.as_deref() == Some(symbol_match.definition_hash.as_str())
                        || definition_hash.is_none()
                    {
                        matches.symbols.push(symbol_match);
                    }
                }
            }
            PatchMatch::Export {
                exported_name,
                module,
                symbol,
                name,
            } => {
                let wanted_symbol =
                    self.resolve_patch_symbol(root, module.as_deref(), symbol, name)?;
                for export in &root.exports {
                    if exported_name
                        .as_deref()
                        .is_none_or(|name| name == export.exported_name)
                        && wanted_symbol
                            .as_deref()
                            .is_none_or(|symbol| symbol == export.symbol)
                    {
                        matches.exports.push(ExportMatch {
                            exported_name: export.exported_name.clone(),
                            symbol_hash: export.symbol.clone(),
                            symbol_name: self.symbol_display(root, &export.symbol)?,
                        });
                    }
                }
            }
            PatchMatch::Type { type_hash, name } => {
                let wanted = self.resolve_patch_type(type_hash.as_deref(), name.as_deref())?;
                self.collect_type_matches(root, &wanted, &mut matches)?;
            }
            PatchMatch::Expr { .. }
            | PatchMatch::LiteralI64 { .. }
            | PatchMatch::LiteralBool { .. }
            | PatchMatch::Call { .. } => {
                self.collect_expression_matches(root, pattern, &mut matches)?;
            }
        }
        Ok(matches)
    }

    fn match_symbols(
        &self,
        root: &ProgramRootPayload,
        module: Option<&str>,
        symbol: &Option<String>,
        name: &Option<String>,
    ) -> Result<Vec<SymbolMatch>> {
        let module = module.unwrap_or(MAIN_BRANCH);
        let wanted_symbol = self.resolve_patch_symbol(root, Some(module), symbol, name)?;
        let mut matches = Vec::new();
        for entry in &root.symbols {
            if wanted_symbol
                .as_deref()
                .is_some_and(|symbol| symbol != entry.symbol)
            {
                continue;
            }
            let Some(binding) = self.preferred_binding(root, &entry.symbol) else {
                continue;
            };
            if binding.module != module {
                continue;
            }
            if name
                .as_deref()
                .is_some_and(|name| name != binding.display_name)
            {
                continue;
            }
            matches.push(SymbolMatch {
                module: binding.module.clone(),
                name: binding.display_name.clone(),
                symbol_hash: entry.symbol.clone(),
                signature_hash: entry.signature.clone(),
                definition_hash: entry.definition.clone(),
            });
        }
        Ok(matches)
    }

    fn collect_type_matches(
        &self,
        root: &ProgramRootPayload,
        wanted_type_hash: &str,
        matches: &mut MatchSet,
    ) -> Result<()> {
        for entry in &root.symbols {
            let symbol_name = self.symbol_display(root, &entry.symbol)?;
            let (params, return_type) = self.signature_parts(&entry.signature)?;
            for type_hash in params.iter().chain(std::iter::once(&return_type)) {
                if type_hash == wanted_type_hash {
                    matches.types.push(TypeMatch {
                        type_hash: type_hash.clone(),
                        type_name: self.type_name(type_hash)?.to_string(),
                        owner_kind: "function_signature".to_string(),
                        owner_hash: entry.signature.clone(),
                        symbol_hash: Some(entry.symbol.clone()),
                        symbol_name: Some(symbol_name.clone()),
                    });
                }
            }
        }
        self.collect_expression_matches(
            root,
            &PatchMatch::Type {
                type_hash: Some(wanted_type_hash.to_string()),
                name: None,
            },
            matches,
        )?;
        Ok(())
    }

    fn collect_expression_matches(
        &self,
        root: &ProgramRootPayload,
        pattern: &PatchMatch,
        matches: &mut MatchSet,
    ) -> Result<()> {
        for entry in &root.symbols {
            if !expression_owner_in_scope(root, entry.symbol.as_str(), pattern) {
                continue;
            }
            let body = self.function_body_hash(&entry.definition)?;
            let symbol_name = self.symbol_display(root, &entry.symbol)?;
            self.visit_patch_expr(
                root,
                &entry.symbol,
                &symbol_name,
                &entry.definition,
                &body,
                pattern,
                matches,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_patch_expr(
        &self,
        root: &ProgramRootPayload,
        owner_symbol: &str,
        owner_name: &str,
        definition_hash: &str,
        expr_hash: &str,
        pattern: &PatchMatch,
        matches: &mut MatchSet,
    ) -> Result<()> {
        let payload = self.get_payload(expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        if self.patch_expr_matches(root, expr_hash, expr_kind, &payload, pattern)? {
            let type_hash = payload
                .get("type")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            if matches!(pattern, PatchMatch::Type { .. }) {
                if let Some(type_hash) = &type_hash {
                    matches.types.push(TypeMatch {
                        type_hash: type_hash.clone(),
                        type_name: self.type_name(type_hash)?.to_string(),
                        owner_kind: "expression".to_string(),
                        owner_hash: expr_hash.to_string(),
                        symbol_hash: Some(owner_symbol.to_string()),
                        symbol_name: Some(owner_name.to_string()),
                    });
                }
            } else {
                matches.expressions.push(ExpressionMatch {
                    module: MAIN_BRANCH.to_string(),
                    symbol_name: owner_name.to_string(),
                    symbol_hash: owner_symbol.to_string(),
                    definition_hash: definition_hash.to_string(),
                    expr_hash: expr_hash.to_string(),
                    expr_kind: expr_kind.to_string(),
                    type_hash,
                    literal_value: expression_literal_value(expr_kind, &payload),
                    target_symbol_hash: payload
                        .get("symbol")
                        .and_then(JsonValue::as_str)
                        .map(str::to_string),
                    target_name: payload
                        .get("symbol")
                        .and_then(JsonValue::as_str)
                        .map(|symbol| self.symbol_display(root, symbol))
                        .transpose()?,
                });
            }
        }

        for child in expression_child_hashes(expr_kind, &payload)? {
            self.visit_patch_expr(
                root,
                owner_symbol,
                owner_name,
                definition_hash,
                &child,
                pattern,
                matches,
            )?;
        }
        Ok(())
    }

    fn patch_expr_matches(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        expr_kind: &str,
        payload: &JsonValue,
        pattern: &PatchMatch,
    ) -> Result<bool> {
        match pattern {
            PatchMatch::Expr {
                expr_hash: wanted_hash,
                expr_kind: wanted_kind,
                ..
            } => Ok(wanted_hash
                .as_deref()
                .is_none_or(|wanted| wanted == expr_hash)
                && wanted_kind
                    .as_deref()
                    .is_none_or(|wanted| wanted == expr_kind)),
            PatchMatch::LiteralI64 { value, .. } => Ok(expr_kind == "literal_i64"
                && payload.get("value").and_then(JsonValue::as_str) == Some(value.as_str())),
            PatchMatch::LiteralBool { value, .. } => Ok(expr_kind == "literal_bool"
                && payload.get("value").and_then(JsonValue::as_bool) == Some(*value)),
            PatchMatch::Call {
                target_symbol,
                target_name,
                target_module,
                ..
            } => {
                if expr_kind != "call" {
                    return Ok(false);
                }
                let wanted = self.resolve_patch_symbol(
                    root,
                    target_module.as_deref(),
                    target_symbol,
                    target_name,
                )?;
                Ok(wanted.as_deref().is_none_or(|wanted| {
                    payload.get("symbol").and_then(JsonValue::as_str) == Some(wanted)
                }))
            }
            PatchMatch::Type { type_hash, name } => {
                let wanted = self.resolve_patch_type(type_hash.as_deref(), name.as_deref())?;
                Ok(payload.get("type").and_then(JsonValue::as_str) == Some(wanted.as_str()))
            }
            PatchMatch::Symbol { .. }
            | PatchMatch::FunctionDefinition { .. }
            | PatchMatch::Export { .. } => Ok(false),
        }
    }

    fn plan_semantic_patch_operations(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        patch: &SemanticPatch,
        matches: &MatchSet,
    ) -> Result<Vec<Operation>> {
        let Some(replace) = &patch.replace else {
            return Ok(Vec::new());
        };
        let Some(pattern) = &patch.match_pattern else {
            bail!("semantic patch requires a match object");
        };
        match replace {
            PatchReplace::LiteralI64 { value } => self.plan_expression_replacement(
                root_hash,
                root,
                matches,
                pattern,
                ExprReplacement::LiteralI64(value.clone()),
            ),
            PatchReplace::LiteralBool { value } => self.plan_expression_replacement(
                root_hash,
                root,
                matches,
                pattern,
                ExprReplacement::LiteralBool(*value),
            ),
            PatchReplace::Unit => self.plan_expression_replacement(
                root_hash,
                root,
                matches,
                pattern,
                ExprReplacement::Unit,
            ),
            PatchReplace::Call {
                target_symbol,
                target_name,
                target_module,
                args,
            } => {
                validate_same_args(args.as_ref())?;
                let symbol = self
                    .resolve_patch_symbol(
                        root,
                        target_module.as_deref(),
                        target_symbol,
                        target_name,
                    )?
                    .ok_or_else(|| {
                        anyhow!("call replacement requires target_symbol or target_name")
                    })?;
                let target_name = self.symbol_display(root, &symbol)?;
                self.plan_expression_replacement(
                    root_hash,
                    root,
                    matches,
                    pattern,
                    ExprReplacement::CallTarget { target_name },
                )
            }
            PatchReplace::RenameSymbol { new_name } => {
                let mut operations = Vec::new();
                for matched in &matches.symbols {
                    operations.push(Operation::RenameSymbol {
                        module: matched.module.clone(),
                        symbol: matched.symbol_hash.clone(),
                        old_name: matched.name.clone(),
                        new_name: new_name.clone(),
                    });
                }
                Ok(operations)
            }
            PatchReplace::ExtractFunction {
                name,
                birth_seed,
                params,
                return_type,
                args,
            } => self.plan_extract_function(
                root,
                matches,
                pattern,
                name,
                birth_seed.as_deref(),
                params,
                return_type.as_deref(),
                args,
            ),
            PatchReplace::InlineFunction => self.plan_inline_function(root, matches, pattern),
            PatchReplace::AddParameter { name, ty, default } => {
                self.plan_add_parameter(matches, name, ty, default.as_ref())
            }
            PatchReplace::RemoveUnusedSymbol => self.plan_remove_unused_symbol(matches),
            PatchReplace::SetExport { exported_name } => {
                let mut operations = Vec::new();
                for matched in matched_symbols_for_export_replace(matches) {
                    operations.push(Operation::SetExport {
                        module: MAIN_BRANCH.to_string(),
                        symbol: matched.symbol_hash,
                        name: matched.name,
                        exported_name: exported_name.clone(),
                    });
                }
                Ok(operations)
            }
            PatchReplace::RemoveExport { exported_name } => {
                let mut operations = Vec::new();
                if matches.exports.is_empty() {
                    for matched in matched_symbols_for_export_replace(matches) {
                        operations.push(Operation::RemoveExport {
                            module: MAIN_BRANCH.to_string(),
                            symbol: matched.symbol_hash,
                            name: matched.name,
                            exported_name: exported_name.clone(),
                        });
                    }
                } else {
                    for matched in &matches.exports {
                        operations.push(Operation::RemoveExport {
                            module: MAIN_BRANCH.to_string(),
                            symbol: matched.symbol_hash.clone(),
                            name: matched.symbol_name.clone(),
                            exported_name: matched.exported_name.clone(),
                        });
                    }
                }
                Ok(operations)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_extract_function(
        &self,
        root: &ProgramRootPayload,
        matches: &MatchSet,
        pattern: &PatchMatch,
        name: &str,
        birth_seed: Option<&str>,
        params: &[ParamSpec],
        return_type: Option<&str>,
        args: &[RawExpr],
    ) -> Result<Vec<Operation>> {
        if matches!(
            pattern,
            PatchMatch::Symbol { .. }
                | PatchMatch::FunctionDefinition { .. }
                | PatchMatch::Type { .. }
                | PatchMatch::Export { .. }
        ) {
            bail!("extract_function requires an expression, literal, or call match");
        }
        if matches.expressions.is_empty() {
            return Ok(Vec::new());
        }
        let unique_exprs = matches
            .expressions
            .iter()
            .map(|matched| matched.expr_hash.clone())
            .collect::<BTreeSet<_>>();
        if unique_exprs.len() != 1 {
            bail!("extract_function requires all matched expressions to have the same expr_hash");
        }
        let matched = matches
            .expressions
            .first()
            .ok_or_else(|| anyhow!("extract_function matched no expressions"))?;
        let return_type = match return_type {
            Some(return_type) => return_type.to_string(),
            None => matched
                .type_hash
                .as_deref()
                .map(|type_hash| self.type_name(type_hash))
                .transpose()?
                .ok_or_else(|| {
                    anyhow!("extract_function requires return_type for untyped match")
                })?,
        };
        let body = self.typed_expr_to_raw(&matched.expr_hash, root)?;
        let mut operations = vec![Operation::CreateFunction {
            module: MAIN_BRANCH.to_string(),
            name: name.to_string(),
            birth_seed: birth_seed
                .map(str::to_string)
                .unwrap_or_else(|| format!("semantic-patch:extract-function:{name}")),
            region_params: Vec::new(),
            params: params.to_vec(),
            return_type,
            effects: Vec::new(),
            body,
        }];
        let replacement = ExprReplacement::NewCall {
            target_name: name.to_string(),
            args: args.to_vec(),
        };
        operations.extend(self.plan_expression_replacement(
            "",
            root,
            matches,
            pattern,
            replacement,
        )?);
        Ok(operations)
    }

    fn plan_inline_function(
        &self,
        root: &ProgramRootPayload,
        matches: &MatchSet,
        pattern: &PatchMatch,
    ) -> Result<Vec<Operation>> {
        if !matches!(pattern, PatchMatch::Call { .. } | PatchMatch::Expr { .. }) {
            bail!("inline_function requires a call or expression match");
        }
        let mut exprs_by_symbol: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for matched in &matches.expressions {
            if matched.expr_kind != "call" {
                bail!(
                    "inline_function matched non-call expression {}",
                    matched.expr_hash
                );
            }
            exprs_by_symbol
                .entry(matched.symbol_hash.clone())
                .or_default()
                .insert(matched.expr_hash.clone());
        }

        let mut operations = Vec::new();
        for (symbol, expr_hashes) in exprs_by_symbol {
            let entry = self
                .root_symbol(root, &symbol)
                .ok_or_else(|| anyhow!("matched symbol missing from root {symbol}"))?;
            let body = self.function_body_hash(&entry.definition)?;
            let name = self.symbol_display(root, &symbol)?;
            let patched =
                self.patched_raw_expr_inline(&body, root, &expr_hashes, &mut Vec::new())?;
            operations.push(Operation::ReplaceFunctionBody {
                module: MAIN_BRANCH.to_string(),
                symbol,
                name,
                body: patched,
            });
        }
        Ok(operations)
    }

    fn patched_raw_expr_inline(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        inline_exprs: &BTreeSet<String>,
        local_names: &mut Vec<String>,
    ) -> Result<RawExpr> {
        let payload = self.get_payload(expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        if inline_exprs.contains(expr_hash) {
            if expr_kind != "call" {
                bail!("inline_function matched non-call expression {expr_hash}");
            }
            return self.inline_call_payload(&payload, root, local_names);
        }

        match expr_kind {
            "literal_i64" => Ok(RawExpr::LiteralI64 {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .to_string(),
            }),
            "literal_bool" => Ok(RawExpr::LiteralBool {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?,
            }),
            "literal_unit" => Ok(RawExpr::Unit),
            "param_ref" => Ok(RawExpr::ParamRef {
                index: payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize,
            }),
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                Ok(RawExpr::ParamName {
                    name: local_at_depth(local_names, depth)
                        .cloned()
                        .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))?,
                })
            }
            "call" => Ok(RawExpr::Call {
                name: self.symbol_display(
                    root,
                    payload
                        .get("symbol")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("call missing symbol"))?,
                )?,
                args: payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                    .iter()
                    .map(|arg| {
                        let hash = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        self.patched_raw_expr_inline(hash, root, inline_exprs, local_names)
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
            "binary" => Ok(RawExpr::Binary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?
                    .to_string(),
                left: Box::new(
                    self.patched_raw_expr_inline(
                        payload
                            .get("left")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing left"))?,
                        root,
                        inline_exprs,
                        local_names,
                    )?,
                ),
                right: Box::new(
                    self.patched_raw_expr_inline(
                        payload
                            .get("right")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing right"))?,
                        root,
                        inline_exprs,
                        local_names,
                    )?,
                ),
            }),
            "unary" => Ok(RawExpr::Unary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?
                    .to_string(),
                expr: Box::new(
                    self.patched_raw_expr_inline(
                        payload
                            .get("expr")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("unary missing expr"))?,
                        root,
                        inline_exprs,
                        local_names,
                    )?,
                ),
            }),
            "let" => {
                let name = payload
                    .get("binding_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_name"))?
                    .to_string();
                let binding_type = payload
                    .get("binding_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_type"))?;
                let value = self.patched_raw_expr_inline(
                    payload
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing value"))?,
                    root,
                    inline_exprs,
                    local_names,
                )?;
                local_names.push(name.clone());
                let body = self.patched_raw_expr_inline(
                    payload
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing body"))?,
                    root,
                    inline_exprs,
                    local_names,
                );
                local_names.pop();
                Ok(RawExpr::Let {
                    name,
                    ty: self.type_name(binding_type)?.to_string(),
                    value: Box::new(value),
                    body: Box::new(body?),
                })
            }
            "if" => Ok(RawExpr::If {
                cond: Box::new(
                    self.patched_raw_expr_inline(
                        payload
                            .get("cond")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing cond"))?,
                        root,
                        inline_exprs,
                        local_names,
                    )?,
                ),
                then_expr: Box::new(
                    self.patched_raw_expr_inline(
                        payload
                            .get("then")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing then"))?,
                        root,
                        inline_exprs,
                        local_names,
                    )?,
                ),
                else_expr: Box::new(
                    self.patched_raw_expr_inline(
                        payload
                            .get("else")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing else"))?,
                        root,
                        inline_exprs,
                        local_names,
                    )?,
                ),
            }),
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn inline_call_payload(
        &self,
        call_payload: &JsonValue,
        root: &ProgramRootPayload,
        caller_locals: &mut Vec<String>,
    ) -> Result<RawExpr> {
        let target_symbol = call_payload
            .get("symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("call missing symbol"))?;
        let target_entry = self
            .root_symbol(root, target_symbol)
            .ok_or_else(|| anyhow!("call target missing from root {target_symbol}"))?;
        let target_body = self.function_body_hash(&target_entry.definition)?;
        let target_raw_body = self.typed_expr_to_raw(&target_body, root)?;
        let empty_replacements = BTreeMap::new();
        let args = call_payload
            .get("args")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("call missing args"))?
            .iter()
            .map(|arg| {
                let hash = arg
                    .as_str()
                    .ok_or_else(|| anyhow!("call arg must be hash"))?;
                self.patched_raw_expr_specific(hash, root, &empty_replacements, caller_locals)
            })
            .collect::<Result<Vec<_>>>()?;

        let mut used_names = caller_locals.iter().cloned().collect::<BTreeSet<_>>();
        for arg in &args {
            collect_free_param_names(arg, &mut Vec::new(), &mut used_names);
        }
        let hygienic_body = alpha_rename_let_bindings(&target_raw_body, &mut used_names);
        substitute_param_refs(&hygienic_body, &args)
    }

    fn plan_add_parameter(
        &self,
        matches: &MatchSet,
        name: &str,
        ty: &str,
        default: Option<&RawExpr>,
    ) -> Result<Vec<Operation>> {
        let mut operations = Vec::new();
        for matched in &matches.symbols {
            operations.push(Operation::AddParameter {
                module: matched.module.clone(),
                symbol: matched.symbol_hash.clone(),
                name: matched.name.clone(),
                param: ParamSpec {
                    name: name.to_string(),
                    ty: ty.to_string(),
                },
                default: default.cloned(),
            });
        }
        Ok(operations)
    }

    fn plan_remove_unused_symbol(&self, matches: &MatchSet) -> Result<Vec<Operation>> {
        let mut operations = Vec::new();
        for matched in &matches.symbols {
            operations.push(Operation::DeleteSymbol {
                module: matched.module.clone(),
                symbol: matched.symbol_hash.clone(),
                name: matched.name.clone(),
                force: false,
            });
        }
        Ok(operations)
    }

    fn plan_expression_replacement(
        &self,
        _root_hash: &str,
        root: &ProgramRootPayload,
        matches: &MatchSet,
        pattern: &PatchMatch,
        replacement: ExprReplacement,
    ) -> Result<Vec<Operation>> {
        if matches!(
            pattern,
            PatchMatch::Symbol { .. }
                | PatchMatch::FunctionDefinition { .. }
                | PatchMatch::Type { .. }
                | PatchMatch::Export { .. }
        ) {
            bail!("expression replacement requires an expression, literal, or call match");
        }
        let mut exprs_by_symbol: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for matched in &matches.expressions {
            exprs_by_symbol
                .entry(matched.symbol_hash.clone())
                .or_default()
                .insert(matched.expr_hash.clone());
        }
        let mut operations = Vec::new();
        for (symbol, expr_hashes) in exprs_by_symbol {
            let entry = self
                .root_symbol(root, &symbol)
                .ok_or_else(|| anyhow!("matched symbol missing from root {symbol}"))?;
            let body = self.function_body_hash(&entry.definition)?;
            let name = self.symbol_display(root, &symbol)?;
            let patched =
                self.patched_raw_expr(&body, root, &expr_hashes, &replacement, &mut Vec::new())?;
            operations.push(Operation::ReplaceFunctionBody {
                module: MAIN_BRANCH.to_string(),
                symbol,
                name,
                body: patched,
            });
        }
        Ok(operations)
    }

    fn patched_raw_expr(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        replacements: &BTreeSet<String>,
        replacement: &ExprReplacement,
        local_names: &mut Vec<String>,
    ) -> Result<RawExpr> {
        let payload = self.get_payload(expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        if replacements.contains(expr_hash) {
            return match replacement {
                ExprReplacement::LiteralI64(value) => Ok(RawExpr::LiteralI64 {
                    value: value.clone(),
                }),
                ExprReplacement::LiteralBool(value) => Ok(RawExpr::LiteralBool { value: *value }),
                ExprReplacement::Unit => Ok(RawExpr::Unit),
                ExprReplacement::CallTarget { target_name } => {
                    if expr_kind != "call" {
                        bail!("call replacement matched non-call expression {expr_hash}");
                    }
                    Ok(RawExpr::Call {
                        name: target_name.clone(),
                        args: payload
                            .get("args")
                            .and_then(JsonValue::as_array)
                            .ok_or_else(|| anyhow!("call missing args"))?
                            .iter()
                            .map(|arg| {
                                let hash = arg
                                    .as_str()
                                    .ok_or_else(|| anyhow!("call arg must be hash"))?;
                                self.patched_raw_expr(
                                    hash,
                                    root,
                                    replacements,
                                    replacement,
                                    local_names,
                                )
                            })
                            .collect::<Result<Vec<_>>>()?,
                    })
                }
                ExprReplacement::NewCall { target_name, args } => Ok(RawExpr::Call {
                    name: target_name.clone(),
                    args: args.clone(),
                }),
            };
        }

        match expr_kind {
            "literal_i64" => Ok(RawExpr::LiteralI64 {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .to_string(),
            }),
            "literal_bool" => Ok(RawExpr::LiteralBool {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?,
            }),
            "literal_unit" => Ok(RawExpr::Unit),
            "param_ref" => Ok(RawExpr::ParamRef {
                index: payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize,
            }),
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                Ok(RawExpr::ParamName {
                    name: local_at_depth(local_names, depth)
                        .cloned()
                        .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))?,
                })
            }
            "call" => Ok(RawExpr::Call {
                name: self.symbol_display(
                    root,
                    payload
                        .get("symbol")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("call missing symbol"))?,
                )?,
                args: payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                    .iter()
                    .map(|arg| {
                        let hash = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        self.patched_raw_expr(hash, root, replacements, replacement, local_names)
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
            "binary" => Ok(RawExpr::Binary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?
                    .to_string(),
                left: Box::new(
                    self.patched_raw_expr(
                        payload
                            .get("left")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing left"))?,
                        root,
                        replacements,
                        replacement,
                        local_names,
                    )?,
                ),
                right: Box::new(
                    self.patched_raw_expr(
                        payload
                            .get("right")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing right"))?,
                        root,
                        replacements,
                        replacement,
                        local_names,
                    )?,
                ),
            }),
            "unary" => Ok(RawExpr::Unary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?
                    .to_string(),
                expr: Box::new(
                    self.patched_raw_expr(
                        payload
                            .get("expr")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("unary missing expr"))?,
                        root,
                        replacements,
                        replacement,
                        local_names,
                    )?,
                ),
            }),
            "let" => {
                let name = payload
                    .get("binding_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_name"))?
                    .to_string();
                let binding_type = payload
                    .get("binding_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_type"))?;
                let value = self.patched_raw_expr(
                    payload
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing value"))?,
                    root,
                    replacements,
                    replacement,
                    local_names,
                )?;
                local_names.push(name.clone());
                let body = self.patched_raw_expr(
                    payload
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing body"))?,
                    root,
                    replacements,
                    replacement,
                    local_names,
                );
                local_names.pop();
                Ok(RawExpr::Let {
                    name,
                    ty: self.type_name(binding_type)?.to_string(),
                    value: Box::new(value),
                    body: Box::new(body?),
                })
            }
            "if" => Ok(RawExpr::If {
                cond: Box::new(
                    self.patched_raw_expr(
                        payload
                            .get("cond")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing cond"))?,
                        root,
                        replacements,
                        replacement,
                        local_names,
                    )?,
                ),
                then_expr: Box::new(
                    self.patched_raw_expr(
                        payload
                            .get("then")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing then"))?,
                        root,
                        replacements,
                        replacement,
                        local_names,
                    )?,
                ),
                else_expr: Box::new(
                    self.patched_raw_expr(
                        payload
                            .get("else")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing else"))?,
                        root,
                        replacements,
                        replacement,
                        local_names,
                    )?,
                ),
            }),
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn patched_raw_expr_specific(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        replacements: &BTreeMap<String, RawExpr>,
        local_names: &mut Vec<String>,
    ) -> Result<RawExpr> {
        if let Some(replacement) = replacements.get(expr_hash) {
            return Ok(replacement.clone());
        }
        let payload = self.get_payload(expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        match expr_kind {
            "literal_i64" => Ok(RawExpr::LiteralI64 {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .to_string(),
            }),
            "literal_bool" => Ok(RawExpr::LiteralBool {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?,
            }),
            "literal_unit" => Ok(RawExpr::Unit),
            "param_ref" => Ok(RawExpr::ParamRef {
                index: payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize,
            }),
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                Ok(RawExpr::ParamName {
                    name: local_at_depth(local_names, depth)
                        .cloned()
                        .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))?,
                })
            }
            "call" => Ok(RawExpr::Call {
                name: self.symbol_display(
                    root,
                    payload
                        .get("symbol")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("call missing symbol"))?,
                )?,
                args: payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                    .iter()
                    .map(|arg| {
                        let hash = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        self.patched_raw_expr_specific(hash, root, replacements, local_names)
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
            "binary" => Ok(RawExpr::Binary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?
                    .to_string(),
                left: Box::new(
                    self.patched_raw_expr_specific(
                        payload
                            .get("left")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing left"))?,
                        root,
                        replacements,
                        local_names,
                    )?,
                ),
                right: Box::new(
                    self.patched_raw_expr_specific(
                        payload
                            .get("right")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing right"))?,
                        root,
                        replacements,
                        local_names,
                    )?,
                ),
            }),
            "unary" => Ok(RawExpr::Unary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?
                    .to_string(),
                expr: Box::new(
                    self.patched_raw_expr_specific(
                        payload
                            .get("expr")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("unary missing expr"))?,
                        root,
                        replacements,
                        local_names,
                    )?,
                ),
            }),
            "let" => {
                let name = payload
                    .get("binding_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_name"))?
                    .to_string();
                let binding_type = payload
                    .get("binding_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_type"))?;
                let value = self.patched_raw_expr_specific(
                    payload
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing value"))?,
                    root,
                    replacements,
                    local_names,
                )?;
                local_names.push(name.clone());
                let body = self.patched_raw_expr_specific(
                    payload
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing body"))?,
                    root,
                    replacements,
                    local_names,
                );
                local_names.pop();
                Ok(RawExpr::Let {
                    name,
                    ty: self.type_name(binding_type)?.to_string(),
                    value: Box::new(value),
                    body: Box::new(body?),
                })
            }
            "if" => Ok(RawExpr::If {
                cond: Box::new(
                    self.patched_raw_expr_specific(
                        payload
                            .get("cond")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing cond"))?,
                        root,
                        replacements,
                        local_names,
                    )?,
                ),
                then_expr: Box::new(
                    self.patched_raw_expr_specific(
                        payload
                            .get("then")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing then"))?,
                        root,
                        replacements,
                        local_names,
                    )?,
                ),
                else_expr: Box::new(
                    self.patched_raw_expr_specific(
                        payload
                            .get("else")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing else"))?,
                        root,
                        replacements,
                        local_names,
                    )?,
                ),
            }),
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn resolve_patch_symbol(
        &self,
        root: &ProgramRootPayload,
        module: Option<&str>,
        symbol: &Option<String>,
        name: &Option<String>,
    ) -> Result<Option<String>> {
        if let Some(symbol) = symbol {
            if self.root_symbol(root, symbol).is_none() {
                bail!("symbol is not present in patch root: {symbol}");
            }
            if let Some(name) = name {
                let module = module.unwrap_or(MAIN_BRANCH);
                let resolved = resolve_name_in_root(root, module, name)
                    .ok_or_else(|| anyhow!("unknown name {module}.{name}"))?;
                if resolved != *symbol {
                    bail!("patch symbol {symbol} does not match name {module}.{name}");
                }
            }
            return Ok(Some(symbol.clone()));
        }
        let Some(name) = name else {
            return Ok(None);
        };
        let module = module.unwrap_or(MAIN_BRANCH);
        Ok(Some(
            resolve_name_in_root(root, module, name)
                .ok_or_else(|| anyhow!("unknown name {module}.{name}"))?,
        ))
    }

    fn resolve_patch_type(&self, type_hash: Option<&str>, name: Option<&str>) -> Result<String> {
        if let Some(type_hash) = type_hash {
            self.type_name(type_hash)?;
            return Ok(type_hash.to_string());
        }
        let Some(name) = name else {
            bail!("type match requires type_hash or name");
        };
        self.type_hash_for_source(name)
    }
}

impl CodeDb {
    fn already_applied_patch_apply_result(
        &self,
        branch: &str,
        current_root_hash: &str,
        current_history_hash: Option<&str>,
        planned_operations: &[Operation],
    ) -> Result<JsonValue> {
        let results = planned_operations
            .iter()
            .map(|operation| {
                Ok(MigrationOutcome::AlreadyApplied(MigrationReport {
                    old_root: current_root_hash.to_string(),
                    new_root: current_root_hash.to_string(),
                    migration_hash: None,
                    history_hash: current_history_hash.map(str::to_string),
                    summary: self.migration_summary_for_roots(
                        operation,
                        current_root_hash,
                        current_root_hash,
                    )?,
                })
                .to_json())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({
            "schema": "codedb/apply-result/v1",
            "status": "already_applied",
            "branch": branch,
            "atomic": true,
            "committed": false,
            "rollback_reason": JsonValue::Null,
            "error": JsonValue::Null,
            "old_root_hash": current_root_hash,
            "new_root_hash": current_root_hash,
            "old_history_hash": current_history_hash,
            "new_history_hash": current_history_hash,
            "history_hash": current_history_hash,
            "operation_count": planned_operations.len(),
            "processed_operation_count": planned_operations.len(),
            "applied_operation_count": 0,
            "operations": results.clone(),
            "results": results,
        }))
    }
}

fn parse_semantic_patch(text: &str) -> Result<SemanticPatch> {
    let value: JsonValue =
        serde_json::from_str(text).context("semantic patch JSON must be a JSON object")?;
    value
        .as_object()
        .ok_or_else(|| anyhow!("semantic patch JSON must be an object"))?;
    serde_json::from_value::<SemanticPatch>(value)
        .context("semantic patch JSON must match codedb/semantic-patch/v1")
}

fn expression_owner_in_scope(
    root: &ProgramRootPayload,
    owner_symbol: &str,
    pattern: &PatchMatch,
) -> bool {
    let (within_symbol, within_name, within_module) = match pattern {
        PatchMatch::Expr {
            within_symbol,
            within_name,
            within_module,
            ..
        }
        | PatchMatch::LiteralI64 {
            within_symbol,
            within_name,
            within_module,
            ..
        }
        | PatchMatch::LiteralBool {
            within_symbol,
            within_name,
            within_module,
            ..
        }
        | PatchMatch::Call {
            within_symbol,
            within_name,
            within_module,
            ..
        } => (within_symbol, within_name, within_module),
        PatchMatch::Type { .. } => return true,
        PatchMatch::Symbol { .. }
        | PatchMatch::FunctionDefinition { .. }
        | PatchMatch::Export { .. } => return false,
    };
    if within_symbol
        .as_deref()
        .is_some_and(|symbol| symbol != owner_symbol)
    {
        return false;
    }
    if let Some(name) = within_name {
        let module = within_module.as_deref().unwrap_or(MAIN_BRANCH);
        return root.names.iter().any(|binding| {
            binding.module == module
                && binding.display_name == *name
                && binding.symbol == owner_symbol
        });
    }
    true
}

fn expression_literal_value(expr_kind: &str, payload: &JsonValue) -> Option<JsonValue> {
    match expr_kind {
        "literal_i64" => payload
            .get("value")
            .and_then(JsonValue::as_str)
            .map(|value| {
                json!({
                    "kind": "i64",
                    "value": value,
                })
            }),
        "literal_bool" => payload
            .get("value")
            .and_then(JsonValue::as_bool)
            .map(|value| {
                json!({
                    "kind": "bool",
                    "value": value,
                })
            }),
        "literal_unit" => Some(json!({ "kind": "unit" })),
        _ => None,
    }
}

fn expression_child_hashes(expr_kind: &str, payload: &JsonValue) -> Result<Vec<String>> {
    let mut children = Vec::new();
    match expr_kind {
        "literal_i64" | "literal_bool" | "literal_unit" | "param_ref" | "local_ref" => {}
        "call" => {
            for arg in payload
                .get("args")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| anyhow!("call missing args"))?
            {
                children.push(
                    arg.as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?
                        .to_string(),
                );
            }
        }
        "binary" => {
            for key in ["left", "right"] {
                children.push(
                    payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("binary missing {key}"))?
                        .to_string(),
                );
            }
        }
        "unary" => {
            children.push(
                payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?
                    .to_string(),
            );
        }
        "let" => {
            for key in ["value", "body"] {
                children.push(
                    payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing {key}"))?
                        .to_string(),
                );
            }
        }
        "if" => {
            for key in ["cond", "then", "else"] {
                children.push(
                    payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("if missing {key}"))?
                        .to_string(),
                );
            }
        }
        other => bail!("unknown expression kind {other}"),
    }
    Ok(children)
}

fn validate_same_args(args: Option<&JsonValue>) -> Result<()> {
    let Some(args) = args else {
        return Ok(());
    };
    if args.as_str() == Some("$same_args") {
        return Ok(());
    }
    bail!("call replacement currently supports only args: \"$same_args\"");
}

fn matched_symbols_for_export_replace(matches: &MatchSet) -> Vec<MatchedSymbolForExport> {
    let mut by_symbol = BTreeMap::new();
    for matched in &matches.symbols {
        by_symbol.insert(
            matched.symbol_hash.clone(),
            MatchedSymbolForExport {
                symbol_hash: matched.symbol_hash.clone(),
                name: matched.name.clone(),
            },
        );
    }
    for matched in &matches.exports {
        by_symbol.insert(
            matched.symbol_hash.clone(),
            MatchedSymbolForExport {
                symbol_hash: matched.symbol_hash.clone(),
                name: matched.symbol_name.clone(),
            },
        );
    }
    by_symbol.into_values().collect()
}

struct MatchedSymbolForExport {
    symbol_hash: String,
    name: String,
}

fn semantic_patch_preview_status(matches: &MatchSet, apply_preview: Option<&JsonValue>) -> String {
    let Some(apply_preview) = apply_preview else {
        return if matches.match_count() == 0 {
            "no_match"
        } else {
            "matched"
        }
        .to_string();
    };
    match apply_preview
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("error")
    {
        "applied" | "already_applied" => "planned",
        "conflict" => "conflict",
        "error" => "error",
        other => other,
    }
    .to_string()
}

fn semantic_patch_apply_status(
    matches: &MatchSet,
    apply_result: Option<&JsonValue>,
    has_replace: bool,
) -> String {
    let Some(apply_result) = apply_result else {
        if matches.match_count() == 0 {
            return "no_match".to_string();
        }
        if !has_replace {
            return "matched".to_string();
        }
        return "no_operation".to_string();
    };
    apply_result
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("error")
        .to_string()
}

fn stale_root_patch_apply_result(
    branch: &str,
    expected_root_hash: &str,
    current_root_hash: &str,
    current_history_hash: Option<&str>,
) -> JsonValue {
    let conflict = json!({
        "status": "conflict",
        "current_root_hash": current_root_hash,
        "expected_root_hash": expected_root_hash,
        "failed_preconditions": ["root_is_current"],
        "failed_postconditions": [],
        "summary": {
            "kind": "semantic_patch",
            "display": "semantic patch apply",
        },
    });
    json!({
        "schema": "codedb/apply-result/v1",
        "status": "conflict",
        "branch": branch,
        "atomic": true,
        "committed": false,
        "rollback_reason": "conflict",
        "error": JsonValue::Null,
        "old_root_hash": current_root_hash,
        "new_root_hash": current_root_hash,
        "old_history_hash": current_history_hash,
        "new_history_hash": current_history_hash,
        "history_hash": current_history_hash,
        "operation_count": 0,
        "processed_operation_count": 0,
        "applied_operation_count": 0,
        "operations": [conflict.clone()],
        "results": [conflict],
    })
}

fn semantic_patch_semantic_summary(
    matches: &MatchSet,
    apply_result: Option<&JsonValue>,
) -> JsonValue {
    let operation_summaries = apply_result
        .and_then(|result| result.get("results"))
        .and_then(JsonValue::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(|result| result.get("summary").cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut operation_kinds = BTreeSet::new();
    let mut changed_symbols = BTreeSet::new();
    for summary in &operation_summaries {
        if let Some(kind) = summary.get("kind").and_then(JsonValue::as_str) {
            operation_kinds.insert(kind.to_string());
        }
    }
    if let Some(results) = apply_result
        .and_then(|result| result.get("results"))
        .and_then(JsonValue::as_array)
    {
        for result in results {
            if result.get("status").and_then(JsonValue::as_str) != Some("applied") {
                continue;
            }
            if let Some(symbol) = result
                .get("summary")
                .and_then(|summary| summary.get("build_impact"))
                .and_then(|impact| impact.get("changed_symbols"))
                .and_then(JsonValue::as_array)
            {
                for symbol in symbol.iter().filter_map(JsonValue::as_str) {
                    changed_symbols.insert(symbol.to_string());
                }
            }
        }
    }
    json!({
        "match_count": matches.match_count(),
        "matched_symbol_count": matches.symbols.len(),
        "matched_expression_count": matches.expressions.len(),
        "matched_type_count": matches.types.len(),
        "matched_export_count": matches.exports.len(),
        "operation_kinds": operation_kinds.into_iter().collect::<Vec<_>>(),
        "changed_symbols": changed_symbols.into_iter().collect::<Vec<_>>(),
        "operation_summaries": operation_summaries,
    })
}

fn semantic_patch_typecheck_status(apply_preview: Option<&JsonValue>) -> JsonValue {
    let Some(apply_preview) = apply_preview else {
        return json!({ "status": "not_run" });
    };
    match apply_preview
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("error")
    {
        "applied" | "already_applied" => json!({ "status": "ok" }),
        "conflict" => json!({
            "status": "not_run",
            "reason": "conflict",
        }),
        "error" => {
            let message = apply_preview_error_message(apply_preview)
                .unwrap_or_else(|| "semantic patch preview failed".to_string());
            json!({
                "status": "error",
                "message": message,
            })
        }
        other => json!({
            "status": "unknown",
            "apply_status": other,
        }),
    }
}

fn semantic_patch_build_impacts(apply_preview: Option<&JsonValue>) -> Vec<JsonValue> {
    apply_preview
        .and_then(|preview| preview.get("results"))
        .and_then(JsonValue::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(|result| {
                    result
                        .get("summary")
                        .and_then(|summary| summary.get("build_impact"))
                        .cloned()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn semantic_patch_conflicts(apply_preview: Option<&JsonValue>) -> Vec<JsonValue> {
    apply_preview
        .and_then(|preview| preview.get("results"))
        .and_then(JsonValue::as_array)
        .map(|results| {
            results
                .iter()
                .filter(|result| {
                    result.get("status").and_then(JsonValue::as_str) == Some("conflict")
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn semantic_patch_diagnostics(
    status: &str,
    matches: &MatchSet,
    apply_preview: Option<&JsonValue>,
) -> Vec<JsonValue> {
    let mut diagnostics = Vec::new();
    if status == "no_match" {
        diagnostics.push(json!({
            "kind": "no_match",
            "message": "semantic patch did not match any root structure",
        }));
    }
    if status == "error" {
        let message = apply_preview
            .and_then(apply_preview_error_message)
            .unwrap_or_else(|| "semantic patch preview failed".to_string());
        let kind = if looks_like_type_error(&message) {
            "type_error"
        } else {
            "invalid_operation"
        };
        diagnostics.push(json!({
            "kind": kind,
            "message": message,
            "details": apply_preview,
        }));
    }
    if status == "conflict" {
        diagnostics.push(json!({
            "kind": "conflict",
            "message": "semantic patch planned operations conflict with the current branch",
            "details": apply_preview,
        }));
    }
    if status == "matched" && matches.match_count() > 0 {
        diagnostics.push(json!({
            "kind": "match_only",
            "message": "semantic patch matched root structure but has no replace object",
        }));
    }
    diagnostics
}

fn looks_like_type_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("type")
        || message.contains("expected i64")
        || message.contains("expected bool")
        || message.contains("expected unit")
        || message.contains("operand expected")
        || message.contains("operand")
        || message.contains("arity")
        || message.contains("return")
        || message.contains("call arg")
}

fn apply_preview_error_message(apply_preview: &JsonValue) -> Option<String> {
    apply_preview
        .get("results")
        .and_then(JsonValue::as_array)
        .and_then(|results| {
            results
                .iter()
                .find(|result| result.get("status").and_then(JsonValue::as_str) == Some("error"))
        })
        .and_then(|result| result.get("error"))
        .or_else(|| apply_preview.get("error"))
        .and_then(JsonValue::as_str)
        .map(str::to_string)
}

fn semantic_patch_agent_metadata(
    patch: &SemanticPatch,
    expected_root_hash: &str,
    matches: &MatchSet,
    planned_operations: &[JsonValue],
) -> Result<JsonValue> {
    let mut agent = match patch.agent.clone().unwrap_or_else(|| json!({})) {
        JsonValue::Object(object) => object,
        other => {
            let mut object = serde_json::Map::new();
            object.insert("agent".to_string(), other);
            object
        }
    };
    let planned_operation_kinds = planned_operations
        .iter()
        .filter_map(|operation| operation.get("kind").and_then(JsonValue::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    agent.insert(
        "semantic_patch".to_string(),
        json!({
            "schema": SEMANTIC_PATCH_PROVENANCE_SCHEMA,
            "patch_schema": SEMANTIC_PATCH_SCHEMA,
            "patch_hash": semantic_patch_hash(patch)?,
            "branch": &patch.branch,
            "expected_root_hash": expected_root_hash,
            "match": serde_json::to_value(&patch.match_pattern)?,
            "replace": serde_json::to_value(&patch.replace)?,
            "match_count": matches.match_count(),
            "matched_symbols": symbol_matches_json(&matches.symbols),
            "matched_expressions": expression_matches_json(&matches.expressions),
            "matched_exports": export_matches_json(&matches.exports),
            "planned_operation_count": planned_operations.len(),
            "planned_operation_kinds": planned_operation_kinds,
        }),
    );
    Ok(JsonValue::Object(agent))
}

fn semantic_patch_hash(patch: &SemanticPatch) -> Result<String> {
    let patch_json = serde_json::to_value(patch)?;
    Ok(hash_bytes(
        SEMANTIC_PATCH_HASH_DOMAIN,
        canonical_json(&patch_json).as_bytes(),
    ))
}

fn symbol_matches_json(matches: &[SymbolMatch]) -> Vec<JsonValue> {
    matches
        .iter()
        .map(|matched| {
            json!({
                "module": matched.module,
                "name": matched.name,
                "symbol_hash": matched.symbol_hash,
                "signature_hash": matched.signature_hash,
                "definition_hash": matched.definition_hash,
            })
        })
        .collect()
}

fn expression_matches_json(matches: &[ExpressionMatch]) -> Vec<JsonValue> {
    matches
        .iter()
        .map(|matched| {
            json!({
                "module": matched.module,
                "symbol_name": matched.symbol_name,
                "symbol_hash": matched.symbol_hash,
                "definition_hash": matched.definition_hash,
                "expr_hash": matched.expr_hash,
                "expr_kind": matched.expr_kind,
                "type_hash": matched.type_hash,
                "literal_value": matched.literal_value,
                "target_symbol_hash": matched.target_symbol_hash,
                "target_name": matched.target_name,
            })
        })
        .collect()
}

fn type_matches_json(matches: &[TypeMatch]) -> Vec<JsonValue> {
    matches
        .iter()
        .map(|matched| {
            json!({
                "type_hash": matched.type_hash,
                "type_name": matched.type_name,
                "owner_kind": matched.owner_kind,
                "owner_hash": matched.owner_hash,
                "symbol_hash": matched.symbol_hash,
                "symbol_name": matched.symbol_name,
            })
        })
        .collect()
}

fn export_matches_json(matches: &[ExportMatch]) -> Vec<JsonValue> {
    matches
        .iter()
        .map(|matched| {
            json!({
                "exported_name": matched.exported_name,
                "symbol_hash": matched.symbol_hash,
                "symbol_name": matched.symbol_name,
            })
        })
        .collect()
}

fn substitute_param_refs(expr: &RawExpr, args: &[RawExpr]) -> Result<RawExpr> {
    Ok(match expr {
        RawExpr::LiteralI64 { value } => RawExpr::LiteralI64 {
            value: value.clone(),
        },
        RawExpr::LiteralBool { value } => RawExpr::LiteralBool { value: *value },
        RawExpr::Unit => RawExpr::Unit,
        RawExpr::ParamRef { index } => args
            .get(*index)
            .cloned()
            .ok_or_else(|| anyhow!("inline_function missing argument for param_ref {index}"))?,
        RawExpr::ParamName { name } => RawExpr::ParamName { name: name.clone() },
        RawExpr::Call {
            name,
            args: call_args,
        } => RawExpr::Call {
            name: name.clone(),
            args: call_args
                .iter()
                .map(|arg| substitute_param_refs(arg, args))
                .collect::<Result<Vec<_>>>()?,
        },
        RawExpr::Binary { op, left, right } => RawExpr::Binary {
            op: op.clone(),
            left: Box::new(substitute_param_refs(left, args)?),
            right: Box::new(substitute_param_refs(right, args)?),
        },
        RawExpr::Unary { op, expr } => RawExpr::Unary {
            op: op.clone(),
            expr: Box::new(substitute_param_refs(expr, args)?),
        },
        RawExpr::BorrowShared { region, target } => RawExpr::BorrowShared {
            region: region.clone(),
            target: Box::new(substitute_param_refs(target, args)?),
        },
        RawExpr::BorrowMut { region, target } => RawExpr::BorrowMut {
            region: region.clone(),
            target: Box::new(substitute_param_refs(target, args)?),
        },
        RawExpr::Assign { target, value } => RawExpr::Assign {
            target: Box::new(substitute_param_refs(target, args)?),
            value: Box::new(substitute_param_refs(value, args)?),
        },
        RawExpr::Let {
            name,
            ty,
            value,
            body,
        } => RawExpr::Let {
            name: name.clone(),
            ty: ty.clone(),
            value: Box::new(substitute_param_refs(value, args)?),
            body: Box::new(substitute_param_refs(body, args)?),
        },
        RawExpr::If {
            cond,
            then_expr,
            else_expr,
        } => RawExpr::If {
            cond: Box::new(substitute_param_refs(cond, args)?),
            then_expr: Box::new(substitute_param_refs(then_expr, args)?),
            else_expr: Box::new(substitute_param_refs(else_expr, args)?),
        },
        RawExpr::Record { fields } => RawExpr::Record {
            fields: fields
                .iter()
                .map(|field| {
                    Ok(RawRecordField {
                        name: field.name.clone(),
                        value: substitute_param_refs(&field.value, args)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        },
        RawExpr::FieldAccess { target, field } => RawExpr::FieldAccess {
            target: Box::new(substitute_param_refs(target, args)?),
            field: field.clone(),
        },
        RawExpr::EnumConstruct {
            enum_type,
            variant,
            value,
        } => RawExpr::EnumConstruct {
            enum_type: enum_type.clone(),
            variant: variant.clone(),
            value: Box::new(substitute_param_refs(value, args)?),
        },
        RawExpr::Case { expr, arms } => RawExpr::Case {
            expr: Box::new(substitute_param_refs(expr, args)?),
            arms: arms
                .iter()
                .map(|arm| {
                    Ok(RawCaseArm {
                        variant: arm.variant.clone(),
                        binding: arm.binding.clone(),
                        body: substitute_param_refs(&arm.body, args)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        },
    })
}

fn collect_free_param_names(
    expr: &RawExpr,
    bound_locals: &mut Vec<String>,
    names: &mut BTreeSet<String>,
) {
    match expr {
        RawExpr::LiteralI64 { .. }
        | RawExpr::LiteralBool { .. }
        | RawExpr::Unit
        | RawExpr::ParamRef { .. } => {}
        RawExpr::ParamName { name } => {
            if !bound_locals.iter().rev().any(|local| local == name) {
                names.insert(name.clone());
            }
        }
        RawExpr::Call { args, .. } => {
            for arg in args {
                collect_free_param_names(arg, bound_locals, names);
            }
        }
        RawExpr::Binary { left, right, .. } => {
            collect_free_param_names(left, bound_locals, names);
            collect_free_param_names(right, bound_locals, names);
        }
        RawExpr::Unary { expr, .. } => collect_free_param_names(expr, bound_locals, names),
        RawExpr::BorrowShared { target, .. } => {
            collect_free_param_names(target, bound_locals, names)
        }
        RawExpr::BorrowMut { target, .. } => collect_free_param_names(target, bound_locals, names),
        RawExpr::Assign { target, value } => {
            collect_free_param_names(target, bound_locals, names);
            collect_free_param_names(value, bound_locals, names);
        }
        RawExpr::Let {
            name, value, body, ..
        } => {
            collect_free_param_names(value, bound_locals, names);
            bound_locals.push(name.clone());
            collect_free_param_names(body, bound_locals, names);
            bound_locals.pop();
        }
        RawExpr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            collect_free_param_names(cond, bound_locals, names);
            collect_free_param_names(then_expr, bound_locals, names);
            collect_free_param_names(else_expr, bound_locals, names);
        }
        RawExpr::Record { fields } => {
            for field in fields {
                collect_free_param_names(&field.value, bound_locals, names);
            }
        }
        RawExpr::FieldAccess { target, .. } => {
            collect_free_param_names(target, bound_locals, names);
        }
        RawExpr::EnumConstruct { value, .. } => {
            collect_free_param_names(value, bound_locals, names);
        }
        RawExpr::Case { expr, arms } => {
            collect_free_param_names(expr, bound_locals, names);
            for arm in arms {
                if let Some(binding) = &arm.binding {
                    bound_locals.push(binding.clone());
                }
                collect_free_param_names(&arm.body, bound_locals, names);
                if arm.binding.is_some() {
                    bound_locals.pop();
                }
            }
        }
    }
}

fn alpha_rename_let_bindings(expr: &RawExpr, used_names: &mut BTreeSet<String>) -> RawExpr {
    alpha_rename_let_bindings_with_scope(expr, used_names, &mut Vec::new())
}

fn alpha_rename_let_bindings_with_scope(
    expr: &RawExpr,
    used_names: &mut BTreeSet<String>,
    renamed_locals: &mut Vec<(String, String)>,
) -> RawExpr {
    match expr {
        RawExpr::LiteralI64 { value } => RawExpr::LiteralI64 {
            value: value.clone(),
        },
        RawExpr::LiteralBool { value } => RawExpr::LiteralBool { value: *value },
        RawExpr::Unit => RawExpr::Unit,
        RawExpr::ParamRef { index } => RawExpr::ParamRef { index: *index },
        RawExpr::ParamName { name } => RawExpr::ParamName {
            name: renamed_locals
                .iter()
                .rev()
                .find_map(|(old, new)| (old == name).then(|| new.clone()))
                .unwrap_or_else(|| name.clone()),
        },
        RawExpr::Call { name, args } => RawExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| alpha_rename_let_bindings_with_scope(arg, used_names, renamed_locals))
                .collect(),
        },
        RawExpr::Binary { op, left, right } => RawExpr::Binary {
            op: op.clone(),
            left: Box::new(alpha_rename_let_bindings_with_scope(
                left,
                used_names,
                renamed_locals,
            )),
            right: Box::new(alpha_rename_let_bindings_with_scope(
                right,
                used_names,
                renamed_locals,
            )),
        },
        RawExpr::Unary { op, expr } => RawExpr::Unary {
            op: op.clone(),
            expr: Box::new(alpha_rename_let_bindings_with_scope(
                expr,
                used_names,
                renamed_locals,
            )),
        },
        RawExpr::BorrowShared { region, target } => RawExpr::BorrowShared {
            region: region.clone(),
            target: Box::new(alpha_rename_let_bindings_with_scope(
                target,
                used_names,
                renamed_locals,
            )),
        },
        RawExpr::BorrowMut { region, target } => RawExpr::BorrowMut {
            region: region.clone(),
            target: Box::new(alpha_rename_let_bindings_with_scope(
                target,
                used_names,
                renamed_locals,
            )),
        },
        RawExpr::Assign { target, value } => RawExpr::Assign {
            target: Box::new(alpha_rename_let_bindings_with_scope(
                target,
                used_names,
                renamed_locals,
            )),
            value: Box::new(alpha_rename_let_bindings_with_scope(
                value,
                used_names,
                renamed_locals,
            )),
        },
        RawExpr::Let {
            name,
            ty,
            value,
            body,
        } => {
            let value = alpha_rename_let_bindings_with_scope(value, used_names, renamed_locals);
            let renamed = unique_inline_local_name(name, used_names);
            used_names.insert(renamed.clone());
            renamed_locals.push((name.clone(), renamed.clone()));
            let body = alpha_rename_let_bindings_with_scope(body, used_names, renamed_locals);
            renamed_locals.pop();
            RawExpr::Let {
                name: renamed,
                ty: ty.clone(),
                value: Box::new(value),
                body: Box::new(body),
            }
        }
        RawExpr::If {
            cond,
            then_expr,
            else_expr,
        } => RawExpr::If {
            cond: Box::new(alpha_rename_let_bindings_with_scope(
                cond,
                used_names,
                renamed_locals,
            )),
            then_expr: Box::new(alpha_rename_let_bindings_with_scope(
                then_expr,
                used_names,
                renamed_locals,
            )),
            else_expr: Box::new(alpha_rename_let_bindings_with_scope(
                else_expr,
                used_names,
                renamed_locals,
            )),
        },
        RawExpr::Record { fields } => RawExpr::Record {
            fields: fields
                .iter()
                .map(|field| RawRecordField {
                    name: field.name.clone(),
                    value: alpha_rename_let_bindings_with_scope(
                        &field.value,
                        used_names,
                        renamed_locals,
                    ),
                })
                .collect(),
        },
        RawExpr::FieldAccess { target, field } => RawExpr::FieldAccess {
            target: Box::new(alpha_rename_let_bindings_with_scope(
                target,
                used_names,
                renamed_locals,
            )),
            field: field.clone(),
        },
        RawExpr::EnumConstruct {
            enum_type,
            variant,
            value,
        } => RawExpr::EnumConstruct {
            enum_type: enum_type.clone(),
            variant: variant.clone(),
            value: Box::new(alpha_rename_let_bindings_with_scope(
                value,
                used_names,
                renamed_locals,
            )),
        },
        RawExpr::Case { expr, arms } => {
            let expr = alpha_rename_let_bindings_with_scope(expr, used_names, renamed_locals);
            let arms = arms
                .iter()
                .map(|arm| {
                    let renamed_binding = arm.binding.as_ref().map(|binding| {
                        let renamed = unique_inline_local_name(binding, used_names);
                        used_names.insert(renamed.clone());
                        renamed_locals.push((binding.clone(), renamed.clone()));
                        renamed
                    });
                    let body =
                        alpha_rename_let_bindings_with_scope(&arm.body, used_names, renamed_locals);
                    if arm.binding.is_some() {
                        renamed_locals.pop();
                    }
                    RawCaseArm {
                        variant: arm.variant.clone(),
                        binding: renamed_binding,
                        body,
                    }
                })
                .collect();
            RawExpr::Case {
                expr: Box::new(expr),
                arms,
            }
        }
    }
}

fn unique_inline_local_name(name: &str, used_names: &BTreeSet<String>) -> String {
    if !used_names.contains(name) {
        return name.to_string();
    }
    let base = format!("{name}_inline");
    if !used_names.contains(&base) {
        return base;
    }
    for idx in 1.. {
        let candidate = format!("{base}_{idx}");
        if !used_names.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn local_at_depth<T>(locals: &[T], depth: usize) -> Option<&T> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
}

fn default_branch() -> String {
    MAIN_BRANCH.to_string()
}
