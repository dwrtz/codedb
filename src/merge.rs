use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde_json::{Value as JsonValue, json};

use crate::migrations::{MergeObjectPayload, MigrationOutcome, MigrationStatus, Operation};
use crate::model::{
    BranchState, ExportBinding, NameBinding, ParamNames, ProgramRootPayload, RootSymbolPayload,
    RootTestBinding, normalize_root,
};
use crate::store::{CodeDb, canonical_json, extract_hash_strings};

const MERGE_RESULT_SCHEMA: &str = "codedb/merge-result/v1";

impl CodeDb {
    pub fn merge_preview_branches(
        &mut self,
        target_branch: &str,
        source_branch: &str,
        json_format: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.merge_preview_branches_in_tx(target_branch, source_branch);
        self.conn.execute_batch("ROLLBACK")?;
        let payload = result?;
        Ok(format_merge_result(&payload, json_format))
    }

    fn merge_preview_branches_in_tx(
        &mut self,
        target_branch: &str,
        source_branch: &str,
    ) -> Result<JsonValue> {
        match self.plan_conservative_merge(target_branch, source_branch)? {
            MergePlanOutcome::Mergeable(plan) => Ok(plan.to_json("preview", "mergeable", false)),
            MergePlanOutcome::Conflict(conflict) => {
                Ok(conflict.to_json("preview", target_branch, source_branch, None, None))
            }
        }
    }

    pub fn merge_apply_branches(
        &mut self,
        target_branch: &str,
        source_branch: &str,
        expected_root: &str,
        json_format: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.merge_apply_branches_in_tx(target_branch, source_branch, expected_root);
        match result {
            Ok((payload, should_commit)) => {
                if should_commit {
                    self.conn.execute_batch("COMMIT")?;
                } else {
                    self.conn.execute_batch("ROLLBACK")?;
                }
                Ok(format_merge_result(&payload, json_format))
            }
            Err(err) => {
                if let Err(rollback_err) = self.conn.execute_batch("ROLLBACK") {
                    return Err(err).context(format!("rollback failed: {rollback_err}"));
                }
                Err(err)
            }
        }
    }

    fn merge_apply_branches_in_tx(
        &mut self,
        target_branch: &str,
        source_branch: &str,
        expected_root: &str,
    ) -> Result<(JsonValue, bool)> {
        let current_target = self.branch(target_branch)?;
        if current_target.root_hash != expected_root {
            let payload = json!({
                "schema": MERGE_RESULT_SCHEMA,
                "mode": "apply",
                "status": "stale_root",
                "target_branch": target_branch,
                "source_branch": source_branch,
                "expected_root_hash": expected_root,
                "actual_root_hash": current_target.root_hash,
                "current_history_hash": current_target.history_hash,
                "committed": false,
                "conflicts": [{
                    "kind": "stale_root",
                    "message": format!("branch {target_branch:?} moved before merge apply"),
                }],
            });
            return Ok((payload, false));
        }

        let plan = match self.plan_conservative_merge(target_branch, source_branch)? {
            MergePlanOutcome::Mergeable(plan) => plan,
            MergePlanOutcome::Conflict(conflict) => {
                return Ok((
                    conflict.to_json(
                        "apply",
                        target_branch,
                        source_branch,
                        Some(expected_root),
                        Some(&current_target),
                    ),
                    false,
                ));
            }
        };

        let operation = plan.to_operation();
        let (outcome, should_commit) = self.apply_and_record_expected_in_tx_on_branch(
            target_branch,
            expected_root,
            operation,
        )?;
        let status = match outcome.status() {
            MigrationStatus::Applied => "merged",
            MigrationStatus::AlreadyApplied => "already_current",
            MigrationStatus::Conflict => "conflict",
        };
        let mut payload = plan.to_json("apply", status, should_commit);
        if let Some(object) = payload.as_object_mut() {
            object.insert("operation_result".to_string(), outcome.to_json());
            object.insert(
                "committed".to_string(),
                JsonValue::Bool(matches!(outcome, MigrationOutcome::Applied(_)) && should_commit),
            );
            match outcome {
                MigrationOutcome::Applied(report) | MigrationOutcome::AlreadyApplied(report) => {
                    object.insert(
                        "old_history_hash".to_string(),
                        report
                            .history_hash
                            .as_ref()
                            .and_then(|_| plan.target_history_hash.clone())
                            .map(JsonValue::String)
                            .unwrap_or(JsonValue::Null),
                    );
                    object.insert(
                        "new_history_hash".to_string(),
                        report
                            .history_hash
                            .as_ref()
                            .map(|hash| JsonValue::String(hash.clone()))
                            .unwrap_or(JsonValue::Null),
                    );
                    object.insert(
                        "history_hash".to_string(),
                        report
                            .history_hash
                            .map(JsonValue::String)
                            .unwrap_or(JsonValue::Null),
                    );
                    object.insert(
                        "migration_hash".to_string(),
                        report
                            .migration_hash
                            .map(JsonValue::String)
                            .unwrap_or(JsonValue::Null),
                    );
                }
                MigrationOutcome::Conflict(conflict) => {
                    let details = MigrationOutcome::Conflict(conflict).to_json();
                    object.insert(
                        "conflicts".to_string(),
                        json!([{
                            "kind": "dependency_conflict",
                            "message": "merge operation failed semantic preconditions",
                            "details": details,
                        }]),
                    );
                }
            }
        }
        Ok((payload, should_commit))
    }

    fn plan_conservative_merge(
        &mut self,
        target_branch: &str,
        source_branch: &str,
    ) -> Result<MergePlanOutcome> {
        let target_state = self.branch(target_branch)?;
        let source_state = self.branch(source_branch)?;
        let Some(ancestor) = self.common_ancestor(target_branch, source_branch)? else {
            return Ok(MergePlanOutcome::Conflict(MergeConflict {
                kind: "dependency_conflict".to_string(),
                message: "branches do not share a replayable semantic ancestor".to_string(),
                symbols: vec![],
                details: json!({
                    "target_branch": target_branch,
                    "source_branch": source_branch,
                    "target_root_hash": target_state.root_hash,
                    "source_root_hash": source_state.root_hash,
                }),
            }));
        };

        let ancestor_root = self.load_root(&ancestor.root_hash)?;
        let target_root = self.load_root(&target_state.root_hash)?;
        let source_root = self.load_root(&source_state.root_hash)?;
        let target_changed_symbols =
            self.changed_symbols_between_roots(&ancestor.root_hash, &target_state.root_hash)?;
        let source_changed_symbols =
            self.changed_symbols_between_roots(&ancestor.root_hash, &source_state.root_hash)?;

        let merged_root = match merge_root_payloads(&ancestor_root, &target_root, &source_root) {
            Ok(root) => root,
            Err(conflict) => return Ok(MergePlanOutcome::Conflict(conflict)),
        };
        let merged_root = normalize_root(merged_root);
        let merged_root_payload = serde_json::to_value(&merged_root)?;
        let object_payloads =
            self.collect_merge_object_payloads(&source_state.root_hash, &merged_root_payload)?;
        let merged_root_hash = self.put_program_root(&merged_root)?;
        if let Err(err) = self
            .index_root(&merged_root_hash)
            .and_then(|_| self.type_check_root(&merged_root_hash))
        {
            return Ok(MergePlanOutcome::Conflict(MergeConflict {
                kind: "dependency_conflict".to_string(),
                message: "merged root failed semantic validation".to_string(),
                symbols: target_changed_symbols
                    .iter()
                    .chain(source_changed_symbols.iter())
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                details: json!({
                    "target_branch": target_branch,
                    "source_branch": source_branch,
                    "target_root_hash": target_state.root_hash,
                    "source_root_hash": source_state.root_hash,
                    "merged_root_hash": merged_root_hash,
                    "error": format!("{err:#}"),
                }),
            }));
        }
        let diff: JsonValue = serde_json::from_str(
            self.diff_roots_json(&target_state.root_hash, &merged_root_hash)?
                .trim_end(),
        )?;

        Ok(MergePlanOutcome::Mergeable(Box::new(MergePlan {
            target_branch: target_branch.to_string(),
            source_branch: source_branch.to_string(),
            ancestor_root_hash: ancestor.root_hash,
            ancestor_history_hash: ancestor.history_hash,
            target_root_hash: target_state.root_hash,
            target_history_hash: target_state.history_hash,
            source_root_hash: source_state.root_hash,
            source_history_hash: source_state.history_hash,
            merged_root_hash,
            merged_root,
            object_payloads,
            target_changed_symbols: target_changed_symbols.into_iter().collect(),
            source_changed_symbols: source_changed_symbols.into_iter().collect(),
            target_unique_migration_count: ancestor.target_unique_migration_count,
            source_unique_migration_count: ancestor.source_unique_migration_count,
            build_impact: diff.get("build_impact").cloned().unwrap_or(JsonValue::Null),
        })))
    }

    fn common_ancestor(
        &self,
        target_branch: &str,
        source_branch: &str,
    ) -> Result<Option<CommonAncestor>> {
        let target_state = self.branch(target_branch)?;
        let source_state = self.branch(source_branch)?;
        let target_points =
            branch_history_points(&self.history_chain(target_branch)?, &target_state);
        let source_points =
            branch_history_points(&self.history_chain(source_branch)?, &source_state);

        let mut best: Option<(usize, usize, CommonAncestor)> = None;
        for target_point in &target_points {
            for source_point in &source_points {
                if target_point.root_hash != source_point.root_hash {
                    continue;
                }
                let score = target_point.sequence + source_point.sequence;
                let history_hash = if target_point.history_hash == source_point.history_hash {
                    target_point.history_hash.clone()
                } else {
                    target_point
                        .history_hash
                        .clone()
                        .or_else(|| source_point.history_hash.clone())
                };
                let ancestor = CommonAncestor {
                    root_hash: target_point.root_hash.clone(),
                    history_hash,
                    target_unique_migration_count: target_points
                        .len()
                        .saturating_sub(1 + target_point.sequence),
                    source_unique_migration_count: source_points
                        .len()
                        .saturating_sub(1 + source_point.sequence),
                };
                if best
                    .as_ref()
                    .is_none_or(|(best_score, _, _)| score > *best_score)
                {
                    best = Some((score, target_point.sequence, ancestor));
                }
            }
        }
        Ok(best.map(|(_, _, ancestor)| ancestor))
    }

    fn changed_symbols_between_roots(
        &self,
        old_root_hash: &str,
        new_root_hash: &str,
    ) -> Result<BTreeSet<String>> {
        let old_root = self.load_root(old_root_hash)?;
        let new_root = self.load_root(new_root_hash)?;
        Ok(changed_symbols_between(&old_root, &new_root))
    }

    fn collect_merge_object_payloads(
        &self,
        source_root_hash: &str,
        merged_root_payload: &JsonValue,
    ) -> Result<Vec<MergeObjectPayload>> {
        let mut stack = vec![source_root_hash.to_string()];
        extract_hash_strings(merged_root_payload, &mut stack);
        let mut seen = BTreeSet::new();
        let mut objects = Vec::new();
        while let Some(hash) = stack.pop() {
            if !seen.insert(hash.clone()) {
                continue;
            }
            let Ok(kind) = self.get_kind(&hash) else {
                continue;
            };
            let payload = self.get_payload(&hash)?;
            extract_hash_strings(&payload, &mut stack);
            objects.push(MergeObjectPayload {
                hash,
                kind,
                payload,
            });
        }
        objects.sort_by(|a, b| a.hash.cmp(&b.hash));
        Ok(objects)
    }
}

enum MergePlanOutcome {
    Mergeable(Box<MergePlan>),
    Conflict(MergeConflict),
}

struct MergePlan {
    target_branch: String,
    source_branch: String,
    ancestor_root_hash: String,
    ancestor_history_hash: Option<String>,
    target_root_hash: String,
    target_history_hash: Option<String>,
    source_root_hash: String,
    source_history_hash: Option<String>,
    merged_root_hash: String,
    merged_root: ProgramRootPayload,
    object_payloads: Vec<MergeObjectPayload>,
    target_changed_symbols: Vec<String>,
    source_changed_symbols: Vec<String>,
    target_unique_migration_count: usize,
    source_unique_migration_count: usize,
    build_impact: JsonValue,
}

impl MergePlan {
    fn to_operation(&self) -> Operation {
        Operation::MergeBranch {
            target_branch: self.target_branch.clone(),
            source_branch: self.source_branch.clone(),
            ancestor_root_hash: self.ancestor_root_hash.clone(),
            ancestor_history_hash: self.ancestor_history_hash.clone(),
            source_root_hash: self.source_root_hash.clone(),
            source_history_hash: self.source_history_hash.clone(),
            merged_root: self.merged_root.clone(),
            object_payloads: self.object_payloads.clone(),
        }
    }

    fn to_json(&self, mode: &str, status: &str, committed: bool) -> JsonValue {
        json!({
            "schema": MERGE_RESULT_SCHEMA,
            "mode": mode,
            "status": status,
            "target_branch": self.target_branch,
            "source_branch": self.source_branch,
            "ancestor_root_hash": self.ancestor_root_hash,
            "ancestor_history_hash": self.ancestor_history_hash,
            "target_root_hash": self.target_root_hash,
            "source_root_hash": self.source_root_hash,
            "old_root_hash": self.target_root_hash,
            "new_root_hash": self.merged_root_hash,
            "merged_root_hash": self.merged_root_hash,
            "target_history_hash": self.target_history_hash,
            "source_history_hash": self.source_history_hash,
            "target_changed_symbols": self.target_changed_symbols,
            "source_changed_symbols": self.source_changed_symbols,
            "target_unique_migration_count": self.target_unique_migration_count,
            "source_unique_migration_count": self.source_unique_migration_count,
            "object_payload_count": self.object_payloads.len(),
            "build_impact": self.build_impact,
            "would_commit": self.merged_root_hash != self.target_root_hash,
            "committed": committed,
            "conflicts": [],
        })
    }
}

#[derive(Debug, Clone)]
struct MergeConflict {
    kind: String,
    message: String,
    symbols: Vec<String>,
    details: JsonValue,
}

impl MergeConflict {
    fn to_json(
        &self,
        mode: &str,
        target_branch: &str,
        source_branch: &str,
        expected_root: Option<&str>,
        current_target: Option<&BranchState>,
    ) -> JsonValue {
        json!({
            "schema": MERGE_RESULT_SCHEMA,
            "mode": mode,
            "status": "conflict",
            "target_branch": target_branch,
            "source_branch": source_branch,
            "expected_root_hash": expected_root,
            "actual_root_hash": current_target.map(|state| state.root_hash.as_str()),
            "current_history_hash": current_target.and_then(|state| state.history_hash.as_deref()),
            "would_commit": false,
            "committed": false,
            "conflicts": [{
                "kind": self.kind,
                "message": self.message,
                "symbols": self.symbols,
                "details": self.details,
            }],
        })
    }
}

#[derive(Clone)]
struct CommonAncestor {
    root_hash: String,
    history_hash: Option<String>,
    target_unique_migration_count: usize,
    source_unique_migration_count: usize,
}

struct HistoryPoint {
    sequence: usize,
    root_hash: String,
    history_hash: Option<String>,
}

fn branch_history_points(
    chain: &[crate::migrations::HistoryItem],
    state: &BranchState,
) -> Vec<HistoryPoint> {
    if chain.is_empty() {
        return vec![HistoryPoint {
            sequence: 0,
            root_hash: state.root_hash.clone(),
            history_hash: state.history_hash.clone(),
        }];
    }

    let mut points = vec![HistoryPoint {
        sequence: 0,
        root_hash: chain[0].input_root.clone(),
        history_hash: None,
    }];
    for (idx, item) in chain.iter().enumerate() {
        points.push(HistoryPoint {
            sequence: idx + 1,
            root_hash: item.output_root.clone(),
            history_hash: Some(item.history_hash.clone()),
        });
    }
    points
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymbolState {
    entry: Option<RootSymbolPayload>,
    names: Vec<NameBinding>,
    param_names: Option<ParamNames>,
    exports: Vec<ExportBinding>,
}

impl SymbolState {
    fn absent() -> Self {
        Self {
            entry: None,
            names: vec![],
            param_names: None,
            exports: vec![],
        }
    }
}

fn merge_root_payloads(
    ancestor: &ProgramRootPayload,
    target: &ProgramRootPayload,
    source: &ProgramRootPayload,
) -> std::result::Result<ProgramRootPayload, MergeConflict> {
    let mut merged = target.clone();
    let all_symbols = root_symbols(ancestor)
        .into_iter()
        .chain(root_symbols(target))
        .chain(root_symbols(source))
        .collect::<BTreeSet<_>>();

    for symbol in all_symbols {
        let base = symbol_state(ancestor, &symbol);
        let left = symbol_state(target, &symbol);
        let right = symbol_state(source, &symbol);
        let left_changed = left != base;
        let right_changed = right != base;
        if !right_changed {
            continue;
        }
        let next = if !left_changed {
            right
        } else if left == right {
            left
        } else {
            merge_symbol_state(&symbol, &base, &left, &right)?
        };
        replace_symbol_state(&mut merged, &symbol, &next);
    }

    if source.tests != ancestor.tests {
        if target.tests == ancestor.tests {
            merged.tests = source.tests.clone();
        } else if target.tests != source.tests {
            return Err(MergeConflict {
                kind: "dependency_conflict".to_string(),
                message: "both branches changed semantic test registry".to_string(),
                symbols: test_entry_symbols(&source.tests),
                details: json!({
                    "facet": "tests",
                }),
            });
        }
    }

    if source.metadata != ancestor.metadata {
        if target.metadata == ancestor.metadata {
            merged.metadata = source.metadata.clone();
        } else if target.metadata != source.metadata {
            return Err(MergeConflict {
                kind: "dependency_conflict".to_string(),
                message: "both branches changed root metadata".to_string(),
                symbols: vec![],
                details: json!({
                    "facet": "metadata",
                }),
            });
        }
    }

    validate_name_conflicts(&merged)?;
    validate_export_conflicts(&merged)?;
    Ok(merged)
}

fn merge_symbol_state(
    symbol: &str,
    base: &SymbolState,
    left: &SymbolState,
    right: &SymbolState,
) -> std::result::Result<SymbolState, MergeConflict> {
    let left_presence_changed = left.entry.is_some() != base.entry.is_some();
    let right_presence_changed = right.entry.is_some() != base.entry.is_some();
    if left_presence_changed || right_presence_changed {
        return Err(symbol_conflict(
            "delete_conflict",
            "one branch removed or added a symbol changed by the other branch",
            symbol,
            "presence",
        ));
    }

    let left_signature_changed = signature_of(left) != signature_of(base);
    let right_signature_changed = signature_of(right) != signature_of(base);
    if left_signature_changed || right_signature_changed {
        if signature_of(left) == signature_of(right)
            && left.names == right.names
            && left.param_names == right.param_names
            && left.exports == right.exports
            && definition_of(left) == definition_of(right)
        {
            return Ok(left.clone());
        }
        return Err(symbol_conflict(
            "signature_conflict",
            "signature changes require an explicit merge",
            symbol,
            "signature",
        ));
    }

    let left_definition_changed = definition_of(left) != definition_of(base);
    let right_definition_changed = definition_of(right) != definition_of(base);
    if left_definition_changed
        && right_definition_changed
        && definition_of(left) != definition_of(right)
    {
        return Err(symbol_conflict(
            "dependency_conflict",
            "both branches changed the same function body",
            symbol,
            "definition",
        ));
    }

    if left.names != base.names && right.names != base.names && left.names != right.names {
        return Err(symbol_conflict(
            "name_conflict",
            "both branches changed names for the same symbol",
            symbol,
            "names",
        ));
    }

    if left.param_names != base.param_names
        && right.param_names != base.param_names
        && left.param_names != right.param_names
    {
        return Err(symbol_conflict(
            "signature_conflict",
            "both branches changed parameter names for the same symbol",
            symbol,
            "param_names",
        ));
    }

    if left.exports != base.exports
        && right.exports != base.exports
        && left.exports != right.exports
    {
        return Err(symbol_conflict(
            "export_conflict",
            "both branches changed exports for the same symbol",
            symbol,
            "exports",
        ));
    }

    Ok(SymbolState {
        entry: if right.entry != base.entry {
            right.entry.clone()
        } else {
            left.entry.clone()
        },
        names: if right.names != base.names {
            right.names.clone()
        } else {
            left.names.clone()
        },
        param_names: if right.param_names != base.param_names {
            right.param_names.clone()
        } else {
            left.param_names.clone()
        },
        exports: if right.exports != base.exports {
            right.exports.clone()
        } else {
            left.exports.clone()
        },
    })
}

fn symbol_conflict(kind: &str, message: &str, symbol: &str, facet: &str) -> MergeConflict {
    MergeConflict {
        kind: kind.to_string(),
        message: message.to_string(),
        symbols: vec![symbol.to_string()],
        details: json!({
            "facet": facet,
        }),
    }
}

fn changed_symbols_between(
    old_root: &ProgramRootPayload,
    new_root: &ProgramRootPayload,
) -> BTreeSet<String> {
    root_symbols(old_root)
        .into_iter()
        .chain(root_symbols(new_root))
        .filter(|symbol| symbol_state(old_root, symbol) != symbol_state(new_root, symbol))
        .collect()
}

fn symbol_state(root: &ProgramRootPayload, symbol: &str) -> SymbolState {
    let Some(entry) = root.symbols.iter().find(|entry| entry.symbol == symbol) else {
        return SymbolState::absent();
    };
    let mut names = root
        .names
        .iter()
        .filter(|binding| binding.symbol == symbol)
        .cloned()
        .collect::<Vec<_>>();
    names.sort_by(|a, b| {
        (&a.module, &a.display_name, a.is_preferred).cmp(&(
            &b.module,
            &b.display_name,
            b.is_preferred,
        ))
    });
    let param_names = root
        .param_names
        .iter()
        .find(|entry| entry.symbol == symbol)
        .cloned();
    let mut exports = root
        .exports
        .iter()
        .filter(|binding| binding.symbol == symbol)
        .cloned()
        .collect::<Vec<_>>();
    exports.sort_by(|a, b| a.exported_name.cmp(&b.exported_name));
    SymbolState {
        entry: Some(entry.clone()),
        names,
        param_names,
        exports,
    }
}

fn replace_symbol_state(root: &mut ProgramRootPayload, symbol: &str, state: &SymbolState) {
    root.symbols.retain(|entry| entry.symbol != symbol);
    root.names.retain(|binding| binding.symbol != symbol);
    root.param_names.retain(|entry| entry.symbol != symbol);
    root.exports.retain(|binding| binding.symbol != symbol);
    if let Some(entry) = &state.entry {
        root.symbols.push(entry.clone());
        root.names.extend(state.names.clone());
        if let Some(param_names) = &state.param_names {
            root.param_names.push(param_names.clone());
        }
        root.exports.extend(state.exports.clone());
    }
}

fn root_symbols(root: &ProgramRootPayload) -> BTreeSet<String> {
    root.symbols
        .iter()
        .map(|entry| entry.symbol.clone())
        .chain(root.names.iter().map(|binding| binding.symbol.clone()))
        .chain(root.param_names.iter().map(|entry| entry.symbol.clone()))
        .chain(root.exports.iter().map(|binding| binding.symbol.clone()))
        .collect()
}

fn signature_of(state: &SymbolState) -> Option<&str> {
    state.entry.as_ref().map(|entry| entry.signature.as_str())
}

fn definition_of(state: &SymbolState) -> Option<&str> {
    state.entry.as_ref().map(|entry| entry.definition.as_str())
}

fn validate_name_conflicts(root: &ProgramRootPayload) -> std::result::Result<(), MergeConflict> {
    let mut names = BTreeMap::<(String, String), String>::new();
    for binding in &root.names {
        let key = (binding.module.clone(), binding.display_name.clone());
        if let Some(existing) = names.insert(key.clone(), binding.symbol.clone())
            && existing != binding.symbol
        {
            return Err(MergeConflict {
                kind: "name_conflict".to_string(),
                message: format!(
                    "name {}.{} is bound to multiple symbols after merge",
                    key.0, key.1
                ),
                symbols: vec![existing, binding.symbol.clone()],
                details: json!({
                    "module": key.0,
                    "name": key.1,
                }),
            });
        }
    }
    Ok(())
}

fn validate_export_conflicts(root: &ProgramRootPayload) -> std::result::Result<(), MergeConflict> {
    let mut exports = BTreeMap::<String, String>::new();
    for binding in &root.exports {
        if let Some(existing) =
            exports.insert(binding.exported_name.clone(), binding.symbol.clone())
            && existing != binding.symbol
        {
            return Err(MergeConflict {
                kind: "export_conflict".to_string(),
                message: format!(
                    "export {} is bound to multiple symbols after merge",
                    binding.exported_name
                ),
                symbols: vec![existing, binding.symbol.clone()],
                details: json!({
                    "exported_name": binding.exported_name,
                }),
            });
        }
    }
    Ok(())
}

fn test_entry_symbols(tests: &[RootTestBinding]) -> Vec<String> {
    tests.iter().map(|binding| binding.test.clone()).collect()
}

fn format_merge_result(value: &JsonValue, json_format: bool) -> String {
    if json_format {
        return format!("{}\n", canonical_json(value));
    }

    let status = value
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown");
    let target = value
        .get("target_branch")
        .and_then(JsonValue::as_str)
        .unwrap_or("target");
    let source = value
        .get("source_branch")
        .and_then(JsonValue::as_str)
        .unwrap_or("source");
    let mut out = format!("merge {status} {target} <- {source}\n");
    for key in [
        "ancestor_root_hash",
        "old_root_hash",
        "new_root_hash",
        "merged_root_hash",
        "migration_hash",
        "history_hash",
        "expected_root_hash",
        "actual_root_hash",
    ] {
        if let Some(value) = value.get(key)
            && !value.is_null()
        {
            out.push_str(&format!("{key} {}\n", display_json_scalar(value)));
        }
    }
    if let Some(conflicts) = value.get("conflicts").and_then(JsonValue::as_array)
        && !conflicts.is_empty()
    {
        for conflict in conflicts {
            out.push_str(&format!(
                "conflict {} {}\n",
                conflict
                    .get("kind")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown"),
                conflict
                    .get("message")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
            ));
        }
    }
    out
}

fn display_json_scalar(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "none".to_string(),
        JsonValue::String(value) => value.clone(),
        other => other.to_string(),
    }
}
