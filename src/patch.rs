use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

use crate::MAIN_BRANCH;
use crate::expr::RawExpr;
use crate::migrations::Operation;
use crate::model::{ProgramRootPayload, resolve_name_in_root};
use crate::store::{CodeDb, canonical_json};

const SEMANTIC_PATCH_SCHEMA: &str = "codedb/semantic-patch/v1";
const SEMANTIC_PATCH_PREVIEW_SCHEMA: &str = "codedb/semantic-patch-preview/v1";

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
    CallTarget { target_name: String },
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
        let mut matches = self.match_semantic_patch(&root_hash, &root, &patch)?;
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

    fn match_semantic_patch(
        &self,
        root_hash: &str,
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
                self.collect_type_matches(root_hash, root, &wanted, &mut matches)?;
            }
            PatchMatch::Expr { .. }
            | PatchMatch::LiteralI64 { .. }
            | PatchMatch::LiteralBool { .. }
            | PatchMatch::Call { .. } => {
                self.collect_expression_matches(root_hash, root, pattern, &mut matches)?;
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
        root_hash: &str,
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
            root_hash,
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
        root_hash: &str,
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
                root_hash,
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
        root_hash: &str,
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
                root_hash,
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
        self.resolve_type(name)
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

fn local_at_depth<T>(locals: &[T], depth: usize) -> Option<&T> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
}

fn default_branch() -> String {
    MAIN_BRANCH.to_string()
}
