use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::abi::validate_exported_abi_name;
use crate::build_plan::{BuildImpact, BuildImpactKind, BuildImpactReason, projection_artifacts};
use crate::expr::RawExpr;
use crate::model::{
    BranchState, ExportBinding, NameBinding, ParamNames, ProgramRootPayload, RootSymbolPayload,
    param_names, root_symbol_index, upsert_param_names, validate_projection_identifier,
};
use crate::store::{CodeDb, canonical_json, hash_bytes};
use crate::types::ParamSpec;
use crate::{HISTORY_DOMAIN, MAIN_BRANCH, MIGRATION_DOMAIN};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Operation {
    CreateFunction {
        module: String,
        name: String,
        birth_seed: String,
        params: Vec<ParamSpec>,
        return_type: String,
        body: RawExpr,
    },
    RenameSymbol {
        module: String,
        symbol: String,
        old_name: String,
        new_name: String,
    },
    ReplaceFunctionBody {
        module: String,
        symbol: String,
        name: String,
        body: RawExpr,
    },
    ChangeFunctionSignature {
        module: String,
        symbol: String,
        name: String,
        params: Vec<ParamSpec>,
        return_type: String,
    },
    DeleteSymbol {
        module: String,
        symbol: String,
        name: String,
        force: bool,
    },
    CreateAlias {
        module: String,
        symbol: String,
        name: String,
        alias: String,
    },
    RemoveAlias {
        module: String,
        symbol: String,
        name: String,
        alias: String,
    },
    SetExport {
        module: String,
        symbol: String,
        name: String,
        exported_name: String,
    },
    RemoveExport {
        module: String,
        symbol: String,
        name: String,
        exported_name: String,
    },
}

impl Operation {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Operation::CreateFunction { .. } => "create_function",
            Operation::RenameSymbol { .. } => "rename_symbol",
            Operation::ReplaceFunctionBody { .. } => "replace_function_body",
            Operation::ChangeFunctionSignature { .. } => "change_function_signature",
            Operation::DeleteSymbol { .. } => "delete_symbol",
            Operation::CreateAlias { .. } => "create_alias",
            Operation::RemoveAlias { .. } => "remove_alias",
            Operation::SetExport { .. } => "set_export",
            Operation::RemoveExport { .. } => "remove_export",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrationStatus {
    Applied,
    AlreadyApplied,
    Conflict,
}

impl MigrationStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MigrationStatus::Applied => "applied",
            MigrationStatus::AlreadyApplied => "already_applied",
            MigrationStatus::Conflict => "conflict",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum MigrationOutcome {
    Applied(MigrationReport),
    AlreadyApplied(MigrationReport),
    Conflict(MigrationConflict),
}

impl MigrationOutcome {
    pub(crate) fn status(&self) -> MigrationStatus {
        match self {
            MigrationOutcome::Applied(_) => MigrationStatus::Applied,
            MigrationOutcome::AlreadyApplied(_) => MigrationStatus::AlreadyApplied,
            MigrationOutcome::Conflict(_) => MigrationStatus::Conflict,
        }
    }

    pub(crate) fn format_cli(&self) -> String {
        match self {
            MigrationOutcome::Applied(report) => report.format_cli(MigrationStatus::Applied),
            MigrationOutcome::AlreadyApplied(report) => {
                report.format_cli(MigrationStatus::AlreadyApplied)
            }
            MigrationOutcome::Conflict(conflict) => conflict.format_cli(),
        }
    }

    pub(crate) fn to_json(&self) -> JsonValue {
        match self {
            MigrationOutcome::Applied(report) => report.to_json(MigrationStatus::Applied),
            MigrationOutcome::AlreadyApplied(report) => {
                report.to_json(MigrationStatus::AlreadyApplied)
            }
            MigrationOutcome::Conflict(conflict) => conflict.to_json(),
        }
    }

    pub(crate) fn format_json(&self) -> String {
        format!("{}\n", canonical_json(&self.to_json()))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationReport {
    pub(crate) old_root: String,
    pub(crate) new_root: String,
    pub(crate) migration_hash: Option<String>,
    pub(crate) history_hash: Option<String>,
    pub(crate) summary: MigrationSummary,
}

impl MigrationReport {
    fn format_cli(&self, status: MigrationStatus) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} {} {}\n",
            status.as_str(),
            self.summary.operation_kind,
            self.summary.display
        ));
        match status {
            MigrationStatus::Applied => {
                out.push_str(&format!("old_root {}\n", self.old_root));
                out.push_str(&format!("new_root {}\n", self.new_root));
                if let Some(migration_hash) = &self.migration_hash {
                    out.push_str(&format!("migration {migration_hash}\n"));
                }
                if let Some(history_hash) = &self.history_hash {
                    out.push_str(&format!("history {history_hash}\n"));
                }
            }
            MigrationStatus::AlreadyApplied => {
                out.push_str(&format!("root {}\n", self.new_root));
                if let Some(history_hash) = &self.history_hash {
                    out.push_str(&format!("history {history_hash}\n"));
                }
            }
            MigrationStatus::Conflict => unreachable!("conflicts use MigrationConflict"),
        }
        self.summary.push_cli_lines(&mut out);
        out
    }

    fn to_json(&self, status: MigrationStatus) -> JsonValue {
        json!({
            "status": status.as_str(),
            "old_root_hash": &self.old_root,
            "new_root_hash": &self.new_root,
            "migration_hash": &self.migration_hash,
            "history_hash": &self.history_hash,
            "summary": self.summary.to_json(),
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationConflict {
    pub(crate) current_root: String,
    pub(crate) expected_root: String,
    pub(crate) summary: MigrationSummary,
    pub(crate) failed_preconditions: Vec<Precondition>,
    pub(crate) failed_postconditions: Vec<Postcondition>,
}

impl MigrationConflict {
    fn format_cli(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} {} {}\n",
            MigrationStatus::Conflict.as_str(),
            self.summary.operation_kind,
            self.summary.display
        ));
        out.push_str(&format!("root {}\n", self.current_root));
        out.push_str(&format!("expected_root {}\n", self.expected_root));
        out.push_str(&format!(
            "failed_preconditions {}\n",
            condition_names(&self.failed_preconditions)
        ));
        out.push_str(&format!(
            "failed_postconditions {}\n",
            condition_names(&self.failed_postconditions)
        ));
        self.summary.push_cli_lines(&mut out);
        out
    }

    fn to_json(&self) -> JsonValue {
        json!({
            "status": MigrationStatus::Conflict.as_str(),
            "current_root_hash": &self.current_root,
            "expected_root_hash": &self.expected_root,
            "failed_preconditions": self.failed_preconditions
                .iter()
                .map(Precondition::kind_name)
                .collect::<Vec<_>>(),
            "failed_postconditions": self.failed_postconditions
                .iter()
                .map(Postcondition::kind_name)
                .collect::<Vec<_>>(),
            "summary": self.summary.to_json(),
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationSummary {
    pub(crate) operation_kind: &'static str,
    pub(crate) display: String,
    pub(crate) semantic_impact: SemanticImpact,
    pub(crate) typecheck: TypecheckImpact,
    pub(crate) build_impact: BuildImpact,
}

impl MigrationSummary {
    fn push_cli_lines(&self, out: &mut String) {
        out.push_str(&format!(
            "semantic_impact {}\n",
            self.semantic_impact.as_str()
        ));
        out.push_str(&format!("typecheck {}\n", self.typecheck.as_str()));
        self.build_impact.push_cli_lines(out);
    }

    fn to_json(&self) -> JsonValue {
        json!({
            "kind": self.operation_kind,
            "display": &self.display,
            "semantic_impact": self.semantic_impact.as_str(),
            "typecheck": self.typecheck.as_str(),
            "build_impact": self.build_impact.to_json(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticImpact {
    FunctionCreated,
    SymbolRenamed,
    ImplementationChanged,
    InterfaceChanged,
    SymbolDeleted,
    AliasCreated,
    AliasRemoved,
    ExportSet,
    ExportRemoved,
}

impl SemanticImpact {
    fn as_str(self) -> &'static str {
        match self {
            SemanticImpact::FunctionCreated => "function_created",
            SemanticImpact::SymbolRenamed => "symbol_renamed",
            SemanticImpact::ImplementationChanged => "implementation_changed",
            SemanticImpact::InterfaceChanged => "interface_changed",
            SemanticImpact::SymbolDeleted => "symbol_deleted",
            SemanticImpact::AliasCreated => "alias_created",
            SemanticImpact::AliasRemoved => "alias_removed",
            SemanticImpact::ExportSet => "export_set",
            SemanticImpact::ExportRemoved => "export_removed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypecheckImpact {
    Checked,
    Unchanged,
    BodyRechecked,
    RootRechecked,
}

impl TypecheckImpact {
    fn as_str(self) -> &'static str {
        match self {
            TypecheckImpact::Checked => "checked",
            TypecheckImpact::Unchanged => "unchanged",
            TypecheckImpact::BodyRechecked => "body_rechecked",
            TypecheckImpact::RootRechecked => "root_rechecked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Precondition {
    RootIsCurrent {
        root: String,
    },
    NameIsAvailable {
        module: String,
        name: String,
    },
    NamePointsToSymbol {
        module: String,
        name: String,
        symbol: String,
    },
    PreferredNamePointsToSymbol {
        module: String,
        name: String,
        symbol: String,
    },
    AliasPointsToSymbol {
        module: String,
        alias: String,
        symbol: String,
    },
    ExportNameIsAvailable {
        name: String,
    },
    ExportPointsToSymbol {
        name: String,
        symbol: String,
    },
}

impl Precondition {
    fn kind_name(&self) -> &'static str {
        match self {
            Precondition::RootIsCurrent { .. } => "root_is_current",
            Precondition::NameIsAvailable { .. } => "name_is_available",
            Precondition::NamePointsToSymbol { .. } => "name_points_to_symbol",
            Precondition::PreferredNamePointsToSymbol { .. } => "preferred_name_points_to_symbol",
            Precondition::AliasPointsToSymbol { .. } => "alias_points_to_symbol",
            Precondition::ExportNameIsAvailable { .. } => "export_name_is_available",
            Precondition::ExportPointsToSymbol { .. } => "export_points_to_symbol",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Postcondition {
    RootExists {
        root: String,
    },
    FunctionSourceMatches {
        module: String,
        name: String,
        params: Vec<ParamSpec>,
        return_type: String,
        body: RawExpr,
    },
    NamePointsToSymbol {
        module: String,
        name: String,
        symbol: String,
    },
    NameAbsent {
        module: String,
        name: String,
    },
    BodySourceMatches {
        module: String,
        name: String,
        symbol: String,
        body: RawExpr,
    },
    SignatureSourceMatches {
        module: String,
        name: String,
        symbol: String,
        params: Vec<ParamSpec>,
        return_type: String,
    },
    SymbolAbsent {
        symbol: String,
    },
    ExportPointsToSymbol {
        name: String,
        symbol: String,
    },
    ExportAbsent {
        name: String,
    },
}

impl Postcondition {
    fn kind_name(&self) -> &'static str {
        match self {
            Postcondition::RootExists { .. } => "root_exists",
            Postcondition::FunctionSourceMatches { .. } => "function_source_matches",
            Postcondition::NamePointsToSymbol { .. } => "name_points_to_symbol",
            Postcondition::NameAbsent { .. } => "name_absent",
            Postcondition::BodySourceMatches { .. } => "body_source_matches",
            Postcondition::SignatureSourceMatches { .. } => "signature_source_matches",
            Postcondition::SymbolAbsent { .. } => "symbol_absent",
            Postcondition::ExportPointsToSymbol { .. } => "export_points_to_symbol",
            Postcondition::ExportAbsent { .. } => "export_absent",
        }
    }
}

trait ConditionName {
    fn condition_name(&self) -> &'static str;
}

impl ConditionName for Precondition {
    fn condition_name(&self) -> &'static str {
        self.kind_name()
    }
}

impl ConditionName for Postcondition {
    fn condition_name(&self) -> &'static str {
        self.kind_name()
    }
}

fn condition_names<T: ConditionName>(conditions: &[T]) -> String {
    if conditions.is_empty() {
        return "none".to_string();
    }
    conditions
        .iter()
        .map(ConditionName::condition_name)
        .collect::<Vec<_>>()
        .join(",")
}

fn operation_summary_parts(op: &Operation) -> (String, SemanticImpact, TypecheckImpact) {
    match op {
        Operation::CreateFunction { module, name, .. } => (
            format!("{module}.{name}"),
            SemanticImpact::FunctionCreated,
            TypecheckImpact::Checked,
        ),
        Operation::RenameSymbol {
            module,
            old_name,
            new_name,
            ..
        } => (
            format!("{module}.{old_name} -> {module}.{new_name}"),
            SemanticImpact::SymbolRenamed,
            TypecheckImpact::Unchanged,
        ),
        Operation::ReplaceFunctionBody { module, name, .. } => (
            format!("{module}.{name}"),
            SemanticImpact::ImplementationChanged,
            TypecheckImpact::BodyRechecked,
        ),
        Operation::ChangeFunctionSignature { module, name, .. } => (
            format!("{module}.{name}"),
            SemanticImpact::InterfaceChanged,
            TypecheckImpact::RootRechecked,
        ),
        Operation::DeleteSymbol { module, name, .. } => (
            format!("{module}.{name}"),
            SemanticImpact::SymbolDeleted,
            TypecheckImpact::RootRechecked,
        ),
        Operation::CreateAlias {
            module,
            name,
            alias,
            ..
        } => (
            format!("{module}.{name} as {module}.{alias}"),
            SemanticImpact::AliasCreated,
            TypecheckImpact::Unchanged,
        ),
        Operation::RemoveAlias {
            module,
            name,
            alias,
            ..
        } => (
            format!("{module}.{name} as {module}.{alias}"),
            SemanticImpact::AliasRemoved,
            TypecheckImpact::Unchanged,
        ),
        Operation::SetExport {
            module,
            name,
            exported_name,
            ..
        } => (
            format!("{module}.{name} as {exported_name}"),
            SemanticImpact::ExportSet,
            TypecheckImpact::Unchanged,
        ),
        Operation::RemoveExport {
            module,
            name,
            exported_name,
            ..
        } => (
            format!("{module}.{name} as {exported_name}"),
            SemanticImpact::ExportRemoved,
            TypecheckImpact::Unchanged,
        ),
    }
}

fn fallback_build_impact(op: &Operation) -> BuildImpact {
    let (kind, recompile_symbols, relink, changed_symbols, reasons) = match op {
        Operation::CreateFunction { .. } => (
            BuildImpactKind::RecompileSymbols,
            vec![],
            true,
            vec![],
            vec![BuildImpactReason::SymbolAdded],
        ),
        Operation::RenameSymbol { .. }
        | Operation::CreateAlias { .. }
        | Operation::RemoveAlias { .. } => (
            BuildImpactKind::MetadataOnly,
            vec![],
            false,
            vec![],
            vec![BuildImpactReason::MetadataChanged],
        ),
        Operation::ReplaceFunctionBody { symbol, .. } => (
            BuildImpactKind::RecompileSymbols,
            vec![symbol.clone()],
            true,
            vec![symbol.clone()],
            vec![
                BuildImpactReason::ImplementationHashChanged,
                BuildImpactReason::BodyExpressionHashChanged,
            ],
        ),
        Operation::ChangeFunctionSignature { symbol, .. } => (
            BuildImpactKind::RecompileDependents,
            vec![symbol.clone()],
            true,
            vec![symbol.clone()],
            vec![BuildImpactReason::InterfaceHashChanged],
        ),
        Operation::DeleteSymbol { symbol, .. } => (
            BuildImpactKind::RelinkOnly,
            vec![],
            true,
            vec![symbol.clone()],
            vec![BuildImpactReason::SymbolRemoved],
        ),
        Operation::SetExport { symbol, .. } | Operation::RemoveExport { symbol, .. } => (
            BuildImpactKind::RelinkOnly,
            vec![],
            true,
            vec![symbol.clone()],
            vec![BuildImpactReason::ExportMapChanged],
        ),
    };
    let projection_artifacts = projection_artifacts();
    let mut artifact_kinds = projection_artifacts.clone();
    if matches!(
        kind,
        BuildImpactKind::RecompileSymbols
            | BuildImpactKind::RecompileDependents
            | BuildImpactKind::FullRebuild
    ) {
        artifact_kinds.push(crate::backend::ArtifactKind::LoweredIr);
        artifact_kinds.push(crate::backend::ArtifactKind::ObjectFile);
    }
    if relink {
        artifact_kinds.push(crate::backend::ArtifactKind::LinkPlan);
        artifact_kinds.push(crate::backend::ArtifactKind::Executable);
    }
    artifact_kinds.sort();
    artifact_kinds.dedup();

    BuildImpact {
        kind,
        artifact_kinds,
        projection_artifacts,
        recompile_symbols,
        relink,
        changed_symbols,
        unchanged_function_defs: vec![],
        direct_dependents: BTreeMap::new(),
        transitive_dependents: BTreeMap::new(),
        reasons,
    }
}

impl CodeDb {
    pub(crate) fn apply_and_record(
        &mut self,
        branch: BranchState,
        op: Operation,
    ) -> Result<MigrationOutcome> {
        let expected_root = branch.root_hash.clone();
        self.apply_and_record_expected(branch, &expected_root, op)
    }

    pub(crate) fn apply_and_record_expected(
        &mut self,
        _branch: BranchState,
        expected_root: &str,
        op: Operation,
    ) -> Result<MigrationOutcome> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.apply_and_record_expected_in_tx(expected_root, op);

        match result {
            Ok((outcome, should_commit)) => {
                if should_commit {
                    self.conn.execute_batch("COMMIT")?;
                } else {
                    self.conn.execute_batch("ROLLBACK")?;
                }
                Ok(outcome)
            }
            Err(err) => {
                if let Err(rollback_err) = self.conn.execute_batch("ROLLBACK") {
                    return Err(err).context(format!("rollback failed: {rollback_err}"));
                }
                Err(err)
            }
        }
    }

    pub(crate) fn apply_and_record_expected_in_tx(
        &mut self,
        expected_root: &str,
        op: Operation,
    ) -> Result<(MigrationOutcome, bool)> {
        let fallback_summary = self.migration_summary(&op);
        let branch = self.branch(MAIN_BRANCH)?;
        let old_root = branch.root_hash.clone();
        let preconditions = self.preconditions_for(expected_root, &op);
        let failed_preconditions = self.failed_preconditions(&old_root, &preconditions)?;
        if !failed_preconditions.is_empty() {
            let current_postconditions = self.postconditions_for(&old_root, &op);
            let failed_postconditions =
                self.failed_postconditions(&old_root, &current_postconditions)?;
            if failed_postconditions.is_empty() {
                let stale_expected_root = failed_preconditions
                    .iter()
                    .any(|precondition| matches!(precondition, Precondition::RootIsCurrent { .. }));
                if stale_expected_root
                    && !self.recorded_operation_output_matches(expected_root, &old_root, &op)?
                {
                    return Ok((
                        MigrationOutcome::Conflict(MigrationConflict {
                            current_root: old_root,
                            expected_root: expected_root.to_string(),
                            summary: fallback_summary.clone(),
                            failed_preconditions,
                            failed_postconditions,
                        }),
                        false,
                    ));
                }
                if !stale_expected_root && !self.recorded_operation_exists(&op)? {
                    return Ok((
                        MigrationOutcome::Conflict(MigrationConflict {
                            current_root: old_root,
                            expected_root: expected_root.to_string(),
                            summary: fallback_summary.clone(),
                            failed_preconditions,
                            failed_postconditions,
                        }),
                        false,
                    ));
                }
                let summary = self.migration_summary_for_roots(&op, &old_root, &old_root)?;
                return Ok((
                    MigrationOutcome::AlreadyApplied(MigrationReport {
                        old_root: old_root.clone(),
                        new_root: old_root,
                        migration_hash: None,
                        history_hash: branch.history_hash,
                        summary,
                    }),
                    false,
                ));
            }

            return Ok((
                MigrationOutcome::Conflict(MigrationConflict {
                    current_root: old_root,
                    expected_root: expected_root.to_string(),
                    summary: fallback_summary.clone(),
                    failed_preconditions,
                    failed_postconditions,
                }),
                false,
            ));
        }

        let new_root =
            self.apply_operation_to_root(&old_root, branch.history_hash.as_deref(), &op)?;
        let postconditions = self.postconditions_for(&new_root, &op);
        let failed_postconditions = self.failed_postconditions(&new_root, &postconditions)?;
        if !failed_postconditions.is_empty() {
            bail!(
                "postcondition failed for {}: {}",
                op.kind_name(),
                condition_names(&failed_postconditions)
            );
        }
        if new_root == old_root {
            let summary = self.migration_summary_for_roots(&op, &old_root, &old_root)?;
            return Ok((
                MigrationOutcome::AlreadyApplied(MigrationReport {
                    old_root: old_root.clone(),
                    new_root: old_root,
                    migration_hash: None,
                    history_hash: branch.history_hash.clone(),
                    summary,
                }),
                false,
            ));
        }
        let operation_json = serde_json::to_value(&op)?;
        let preconditions_json = serde_json::to_value(&preconditions)?;
        let postconditions_json = serde_json::to_value(&postconditions)?;
        let migration_hash = migration_hash(
            branch.history_hash.as_deref(),
            &old_root,
            &new_root,
            &operation_json,
            &preconditions_json,
            &postconditions_json,
        );
        let history_hash = history_hash(branch.history_hash.as_deref(), &migration_hash, &new_root);
        let summary = self.migration_summary_for_roots(&op, &old_root, &new_root)?;

        self.conn.execute(
            "INSERT OR IGNORE INTO migrations
             (hash, parent_history_hash, input_root_hash, output_root_hash,
              operation_kind, operation_json, preconditions_json, postconditions_json, agent_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '{}')",
            params![
                migration_hash,
                branch.history_hash,
                old_root,
                new_root,
                op.kind_name(),
                canonical_json(&operation_json),
                canonical_json(&preconditions_json),
                canonical_json(&postconditions_json),
            ],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO histories
             (history_hash, parent_history_hash, migration_hash, output_root_hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![history_hash, branch.history_hash, migration_hash, new_root],
        )?;
        self.update_branch(MAIN_BRANCH, &new_root, &history_hash)?;
        Ok((
            MigrationOutcome::Applied(MigrationReport {
                old_root,
                new_root,
                migration_hash: Some(migration_hash),
                history_hash: Some(history_hash),
                summary,
            }),
            true,
        ))
    }

    pub(crate) fn preconditions_for(&self, input_root: &str, op: &Operation) -> Vec<Precondition> {
        match op {
            Operation::CreateFunction { module, name, .. } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::NameIsAvailable {
                    module: module.clone(),
                    name: name.clone(),
                },
            ],
            Operation::RenameSymbol {
                module,
                symbol,
                old_name,
                new_name,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::PreferredNamePointsToSymbol {
                    module: module.clone(),
                    name: old_name.clone(),
                    symbol: symbol.clone(),
                },
                Precondition::NameIsAvailable {
                    module: module.clone(),
                    name: new_name.clone(),
                },
            ],
            Operation::ReplaceFunctionBody {
                module,
                symbol,
                name,
                ..
            }
            | Operation::ChangeFunctionSignature {
                module,
                symbol,
                name,
                ..
            }
            | Operation::DeleteSymbol {
                module,
                symbol,
                name,
                ..
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::NamePointsToSymbol {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
                },
            ],
            Operation::CreateAlias {
                module,
                symbol,
                name,
                alias,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::NamePointsToSymbol {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
                },
                Precondition::NameIsAvailable {
                    module: module.clone(),
                    name: alias.clone(),
                },
            ],
            Operation::RemoveAlias {
                module,
                symbol,
                name,
                alias,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::NamePointsToSymbol {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
                },
                Precondition::AliasPointsToSymbol {
                    module: module.clone(),
                    alias: alias.clone(),
                    symbol: symbol.clone(),
                },
            ],
            Operation::SetExport {
                module,
                symbol,
                name,
                exported_name,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::NamePointsToSymbol {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
                },
                Precondition::ExportNameIsAvailable {
                    name: exported_name.clone(),
                },
            ],
            Operation::RemoveExport {
                module,
                symbol,
                name,
                exported_name,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::NamePointsToSymbol {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
                },
                Precondition::ExportPointsToSymbol {
                    name: exported_name.clone(),
                    symbol: symbol.clone(),
                },
            ],
        }
    }

    pub(crate) fn postconditions_for(
        &self,
        output_root: &str,
        op: &Operation,
    ) -> Vec<Postcondition> {
        match op {
            Operation::CreateFunction {
                module,
                name,
                params,
                return_type,
                body,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::FunctionSourceMatches {
                    module: module.clone(),
                    name: name.clone(),
                    params: params.clone(),
                    return_type: return_type.clone(),
                    body: body.clone(),
                },
            ],
            Operation::RenameSymbol {
                module,
                symbol,
                old_name,
                new_name,
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::NamePointsToSymbol {
                    module: module.clone(),
                    name: new_name.clone(),
                    symbol: symbol.clone(),
                },
                Postcondition::NameAbsent {
                    module: module.clone(),
                    name: old_name.clone(),
                },
            ],
            Operation::ReplaceFunctionBody {
                module,
                symbol,
                name,
                body,
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::BodySourceMatches {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
                    body: body.clone(),
                },
            ],
            Operation::ChangeFunctionSignature {
                module,
                symbol,
                name,
                params,
                return_type,
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::SignatureSourceMatches {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
                    params: params.clone(),
                    return_type: return_type.clone(),
                },
            ],
            Operation::DeleteSymbol { symbol, .. } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::SymbolAbsent {
                    symbol: symbol.clone(),
                },
            ],
            Operation::CreateAlias {
                module,
                symbol,
                alias,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::NamePointsToSymbol {
                    module: module.clone(),
                    name: alias.clone(),
                    symbol: symbol.clone(),
                },
            ],
            Operation::RemoveAlias { module, alias, .. } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::NameAbsent {
                    module: module.clone(),
                    name: alias.clone(),
                },
            ],
            Operation::SetExport {
                symbol,
                exported_name,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::ExportPointsToSymbol {
                    name: exported_name.clone(),
                    symbol: symbol.clone(),
                },
            ],
            Operation::RemoveExport { exported_name, .. } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::ExportAbsent {
                    name: exported_name.clone(),
                },
            ],
        }
    }

    pub(crate) fn migration_summary(&self, op: &Operation) -> MigrationSummary {
        let (display, semantic_impact, typecheck) = operation_summary_parts(op);
        let build_impact = fallback_build_impact(op);

        MigrationSummary {
            operation_kind: op.kind_name(),
            display,
            semantic_impact,
            typecheck,
            build_impact,
        }
    }

    pub(crate) fn migration_summary_for_roots(
        &self,
        op: &Operation,
        old_root: &str,
        new_root: &str,
    ) -> Result<MigrationSummary> {
        let (display, semantic_impact, typecheck) = operation_summary_parts(op);
        let build_impact = self.plan_build_impact(old_root, new_root)?;
        Ok(MigrationSummary {
            operation_kind: op.kind_name(),
            display,
            semantic_impact,
            typecheck,
            build_impact,
        })
    }

    pub(crate) fn failed_preconditions(
        &self,
        current_root: &str,
        preconditions: &[Precondition],
    ) -> Result<Vec<Precondition>> {
        let root = self.load_root(current_root)?;
        let mut failed = Vec::new();
        for precondition in preconditions {
            let holds = match precondition {
                Precondition::RootIsCurrent { root } => root == current_root,
                Precondition::NameIsAvailable { module, name } => !root
                    .names
                    .iter()
                    .any(|binding| binding.module == *module && binding.display_name == *name),
                Precondition::NamePointsToSymbol {
                    module,
                    name,
                    symbol,
                } => name_points_to_symbol(&root, module, name, symbol),
                Precondition::PreferredNamePointsToSymbol {
                    module,
                    name,
                    symbol,
                } => preferred_name_points_to_symbol(&root, module, name, symbol),
                Precondition::AliasPointsToSymbol {
                    module,
                    alias,
                    symbol,
                } => alias_points_to_symbol(&root, module, alias, symbol),
                Precondition::ExportNameIsAvailable { name } => !root
                    .exports
                    .iter()
                    .any(|binding| binding.exported_name == *name),
                Precondition::ExportPointsToSymbol { name, symbol } => {
                    export_points_to_symbol(&root, name, symbol)
                }
            };
            if !holds {
                failed.push(precondition.clone());
            }
        }
        Ok(failed)
    }

    pub(crate) fn failed_postconditions(
        &self,
        current_root: &str,
        postconditions: &[Postcondition],
    ) -> Result<Vec<Postcondition>> {
        let root = self.load_root(current_root)?;
        let mut failed = Vec::new();
        for postcondition in postconditions {
            if !self.postcondition_holds(current_root, &root, postcondition)? {
                failed.push(postcondition.clone());
            }
        }
        Ok(failed)
    }

    fn postcondition_holds(
        &self,
        current_root: &str,
        root: &ProgramRootPayload,
        postcondition: &Postcondition,
    ) -> Result<bool> {
        match postcondition {
            Postcondition::RootExists { root } => {
                if root == current_root {
                    return Ok(true);
                }
                let exists: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                    params![root],
                    |row| row.get(0),
                )?;
                Ok(exists)
            }
            Postcondition::FunctionSourceMatches {
                module,
                name,
                params,
                return_type,
                body,
            } => {
                let Some(symbol) = symbol_for_name(root, module, name) else {
                    return Ok(false);
                };
                let param_names = params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect::<Vec<_>>();
                Ok(
                    self.function_signature_source_matches(root, &symbol, params, return_type)?
                        && self.function_body_source_matches(root, &symbol, body, &param_names)?,
                )
            }
            Postcondition::NamePointsToSymbol {
                module,
                name,
                symbol,
            } => Ok(name_points_to_symbol(root, module, name, symbol)),
            Postcondition::NameAbsent { module, name } => Ok(!root
                .names
                .iter()
                .any(|binding| binding.module == *module && binding.display_name == *name)),
            Postcondition::BodySourceMatches {
                module,
                name,
                symbol,
                body,
            } => {
                if !name_points_to_symbol(root, module, name, symbol) {
                    return Ok(false);
                }
                self.function_body_source_matches(root, symbol, body, &param_names(root, symbol))
            }
            Postcondition::SignatureSourceMatches {
                module,
                name,
                symbol,
                params,
                return_type,
            } => {
                if !name_points_to_symbol(root, module, name, symbol) {
                    return Ok(false);
                }
                self.function_signature_source_matches(root, symbol, params, return_type)
            }
            Postcondition::SymbolAbsent { symbol } => {
                Ok(!root.symbols.iter().any(|entry| entry.symbol == *symbol)
                    && !root.names.iter().any(|binding| binding.symbol == *symbol)
                    && !root.exports.iter().any(|binding| binding.symbol == *symbol))
            }
            Postcondition::ExportPointsToSymbol { name, symbol } => {
                Ok(export_points_to_symbol(root, name, symbol))
            }
            Postcondition::ExportAbsent { name } => Ok(!root
                .exports
                .iter()
                .any(|binding| binding.exported_name == *name)),
        }
    }

    fn function_signature_source_matches(
        &self,
        root: &ProgramRootPayload,
        symbol: &str,
        params: &[ParamSpec],
        return_type: &str,
    ) -> Result<bool> {
        let Some(entry) = self.root_symbol(root, symbol) else {
            return Ok(false);
        };
        let (actual_params, actual_return_type) = self.signature_parts(&entry.signature)?;
        let expected_params = params
            .iter()
            .map(|param| self.resolve_type(&param.ty))
            .collect::<Result<Vec<_>>>()?;
        let expected_return_type = self.resolve_type(return_type)?;
        let expected_names = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        Ok(actual_params == expected_params
            && actual_return_type == expected_return_type
            && param_names(root, symbol) == expected_names)
    }

    fn function_body_source_matches(
        &self,
        root: &ProgramRootPayload,
        symbol: &str,
        expected_body: &RawExpr,
        local_params: &[String],
    ) -> Result<bool> {
        let Some(entry) = self.root_symbol(root, symbol) else {
            return Ok(false);
        };
        let body = self.function_body_hash(&entry.definition)?;
        let actual = self.typed_expr_to_raw(&body, root)?;
        let expected = normalize_param_refs(expected_body, local_params);
        Ok(actual == expected)
    }

    pub(crate) fn apply_operation_to_root(
        &mut self,
        input_root: &str,
        parent_history_hash: Option<&str>,
        op: &Operation,
    ) -> Result<String> {
        match op {
            Operation::CreateFunction {
                module,
                name,
                birth_seed,
                params,
                return_type,
                body,
            } => self.apply_create_function(
                input_root,
                parent_history_hash,
                module,
                name,
                birth_seed,
                params,
                return_type,
                body,
            ),
            Operation::RenameSymbol {
                module,
                symbol,
                old_name,
                new_name,
            } => self.apply_rename_symbol(input_root, module, symbol, old_name, new_name),
            Operation::ReplaceFunctionBody {
                module,
                symbol,
                name,
                body,
            } => self.apply_replace_body(input_root, module, symbol, name, body),
            Operation::ChangeFunctionSignature {
                module,
                symbol,
                name,
                params,
                return_type,
            } => self.apply_change_signature(input_root, module, symbol, name, params, return_type),
            Operation::DeleteSymbol {
                module,
                symbol,
                name,
                force,
            } => self.apply_delete_symbol(input_root, module, symbol, name, *force),
            Operation::CreateAlias {
                module,
                symbol,
                name,
                alias,
            } => self.apply_create_alias(input_root, module, symbol, name, alias),
            Operation::RemoveAlias {
                module,
                symbol,
                name,
                alias,
            } => self.apply_remove_alias(input_root, module, symbol, name, alias),
            Operation::SetExport {
                module,
                symbol,
                name,
                exported_name,
            } => self.apply_set_export(input_root, module, symbol, name, exported_name),
            Operation::RemoveExport {
                module,
                symbol,
                name,
                exported_name,
            } => self.apply_remove_export(input_root, module, symbol, name, exported_name),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_create_function(
        &mut self,
        input_root: &str,
        parent_history_hash: Option<&str>,
        module: &str,
        name: &str,
        birth_seed: &str,
        params: &[ParamSpec],
        return_type: &str,
        body: &RawExpr,
    ) -> Result<String> {
        validate_projection_identifier("function name", name)?;
        validate_param_names(params)?;
        let mut root = self.load_root(input_root)?;
        if root
            .names
            .iter()
            .any(|binding| binding.module == module && binding.display_name == name)
        {
            bail!("name already exists: {module}.{name}");
        }

        let symbol = self.put_symbol_birth(parent_history_hash, birth_seed)?;
        let param_types = params
            .iter()
            .map(|param| self.resolve_type(&param.ty))
            .collect::<Result<Vec<_>>>()?;
        let return_type_hash = self.resolve_type(return_type)?;
        let signature = self.put_signature(&param_types, &return_type_hash)?;
        let param_name_list = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        let typed_body = self.type_expr(body, &root, &param_name_list, &param_types)?;
        if typed_body.type_hash != return_type_hash {
            bail!(
                "function {module}.{name} body type {} does not match return type {}",
                self.type_name(&typed_body.type_hash)?,
                self.type_name(&return_type_hash)?
            );
        }
        let definition = self.put_function_def(&symbol, &signature, &typed_body.expr_hash)?;

        root.symbols.push(RootSymbolPayload {
            symbol: symbol.clone(),
            definition,
            signature: signature.clone(),
        });
        root.names.push(NameBinding {
            module: module.to_string(),
            display_name: name.to_string(),
            symbol: symbol.clone(),
            is_preferred: true,
        });
        root.param_names.push(ParamNames {
            symbol,
            names: param_name_list,
        });
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_rename_symbol(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<String> {
        validate_projection_identifier("function name", new_name)?;
        let mut root = self.load_root(input_root)?;
        if root
            .names
            .iter()
            .any(|binding| binding.module == module && binding.display_name == new_name)
        {
            bail!("name already exists: {module}.{new_name}");
        }
        let mut changed = false;
        for binding in &mut root.names {
            if binding.module == module
                && binding.display_name == old_name
                && binding.symbol == symbol
                && binding.is_preferred
            {
                binding.display_name = new_name.to_string();
                changed = true;
            }
        }
        if !changed {
            bail!("precondition failed: {module}.{old_name} does not point to {symbol}");
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_replace_body(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        body: &RawExpr,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let idx = root_symbol_index(&root, symbol)?;
        let signature = root.symbols[idx].signature.clone();
        let (param_types, return_type) = self.signature_parts(&signature)?;
        let param_name_list = param_names(&root, symbol);
        let typed_body = self.type_expr(body, &root, &param_name_list, &param_types)?;
        if typed_body.type_hash != return_type {
            bail!(
                "replacement body type {} does not match return type {}",
                self.type_name(&typed_body.type_hash)?,
                self.type_name(&return_type)?
            );
        }
        let definition = self.put_function_def(symbol, &signature, &typed_body.expr_hash)?;
        root.symbols[idx].definition = definition;
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_change_signature(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        params: &[ParamSpec],
        return_type: &str,
    ) -> Result<String> {
        validate_param_names(params)?;
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let idx = root_symbol_index(&root, symbol)?;
        let old_definition = root.symbols[idx].definition.clone();
        let old_body_hash = self.function_body_hash(&old_definition)?;
        let raw_body = self.typed_expr_to_raw(&old_body_hash, &root)?;
        let param_types = params
            .iter()
            .map(|param| self.resolve_type(&param.ty))
            .collect::<Result<Vec<_>>>()?;
        let return_type_hash = self.resolve_type(return_type)?;
        let signature = self.put_signature(&param_types, &return_type_hash)?;
        let param_name_list = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        let typed_body = self.type_expr(&raw_body, &root, &param_name_list, &param_types)?;
        if typed_body.type_hash != return_type_hash {
            bail!(
                "body type {} does not match new return type {}",
                self.type_name(&typed_body.type_hash)?,
                self.type_name(&return_type_hash)?
            );
        }
        let definition = self.put_function_def(symbol, &signature, &typed_body.expr_hash)?;
        root.symbols[idx].signature = signature;
        root.symbols[idx].definition = definition;
        upsert_param_names(&mut root, symbol, param_name_list);
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)
            .context("new signature invalidates existing root")?;
        Ok(new_root)
    }

    pub(crate) fn apply_delete_symbol(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        force: bool,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let deps = self.reverse_dependencies_for_root(&root, symbol)?;
        if !force && !deps.is_empty() {
            let names = deps
                .into_iter()
                .map(|dep| self.symbol_display(&root, &dep))
                .collect::<Result<Vec<_>>>()?;
            bail!(
                "cannot delete {module}.{name}; live callers: {}",
                names.join(", ")
            );
        }
        root.symbols.retain(|entry| entry.symbol != symbol);
        root.names.retain(|binding| binding.symbol != symbol);
        root.param_names.retain(|entry| entry.symbol != symbol);
        root.exports.retain(|binding| binding.symbol != symbol);
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_create_alias(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        alias: &str,
    ) -> Result<String> {
        validate_projection_identifier("alias", alias)?;
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        if root
            .names
            .iter()
            .any(|binding| binding.module == module && binding.display_name == alias)
        {
            bail!("name already exists: {module}.{alias}");
        }
        root.names.push(NameBinding {
            module: module.to_string(),
            display_name: alias.to_string(),
            symbol: symbol.to_string(),
            is_preferred: false,
        });
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_remove_alias(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        alias: &str,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let original_len = root.names.len();
        root.names.retain(|binding| {
            !(binding.module == module
                && binding.display_name == alias
                && binding.symbol == symbol
                && !binding.is_preferred)
        });
        if root.names.len() == original_len {
            bail!("alias {module}.{alias} does not point to {symbol}");
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_set_export(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        exported_name: &str,
    ) -> Result<String> {
        validate_exported_abi_name(exported_name)?;
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        if root
            .exports
            .iter()
            .any(|binding| binding.exported_name == exported_name)
        {
            bail!("export already exists: {exported_name}");
        }
        root.exports.push(ExportBinding {
            symbol: symbol.to_string(),
            exported_name: exported_name.to_string(),
        });
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_remove_export(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        exported_name: &str,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let original_len = root.exports.len();
        root.exports.retain(|binding| {
            !(binding.exported_name == exported_name && binding.symbol == symbol)
        });
        if root.exports.len() == original_len {
            bail!("export {exported_name} does not point to {symbol}");
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn assert_name_points(
        &self,
        root: &ProgramRootPayload,
        module: &str,
        name: &str,
        symbol: &str,
    ) -> Result<()> {
        if root.names.iter().any(|binding| {
            binding.module == module && binding.display_name == name && binding.symbol == symbol
        }) {
            Ok(())
        } else {
            bail!("precondition failed: {module}.{name} does not point to {symbol}")
        }
    }

    pub fn history_main_branch(&self) -> Result<String> {
        let chain = self.history_chain(MAIN_BRANCH)?;
        let mut out = String::new();
        for item in chain {
            out.push_str(&format!(
                "{} {} -> {}\n  migration {}\n  history {}\n",
                item.operation_kind,
                item.input_root,
                item.output_root,
                item.migration_hash,
                item.history_hash
            ));
        }
        if out.is_empty() {
            out.push_str("history empty\n");
        }
        Ok(out)
    }

    pub fn history_main_branch_json(&self) -> Result<String> {
        let branch = self.branch(MAIN_BRANCH)?;
        let chain = self.history_chain(MAIN_BRANCH)?;
        let migrations = chain
            .into_iter()
            .map(|item| {
                Ok(json!({
                    "operation_kind": item.operation_kind,
                    "input_root_hash": item.input_root,
                    "output_root_hash": item.output_root,
                    "migration_hash": item.migration_hash,
                    "history_hash": item.history_hash,
                    "operation": serde_json::to_value(item.operation)?,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(format!(
            "{}\n",
            canonical_json(&json!({
                "branch": MAIN_BRANCH,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
                "migrations": migrations,
            }))
        ))
    }

    pub fn replay_main_branch(&mut self) -> Result<String> {
        self.ensure_initialized()?;
        let expected = self.branch(MAIN_BRANCH)?;
        let chain = self.history_chain(MAIN_BRANCH)?;
        let mut current_root = self.put_program_root(&ProgramRootPayload {
            symbols: vec![],
            names: vec![],
            param_names: vec![],
            exports: vec![],
            metadata: BTreeMap::new(),
        })?;
        let mut current_history: Option<String> = None;

        for item in &chain {
            if item.input_root != current_root {
                bail!(
                    "bad_history_link: migration {} expected input {}, replay has {}",
                    item.migration_hash,
                    item.input_root,
                    current_root
                );
            }
            let preconditions = self.preconditions_for(&current_root, &item.operation);
            let failed_preconditions = self.failed_preconditions(&current_root, &preconditions)?;
            let produced = if failed_preconditions.is_empty() {
                let produced = self
                    .apply_operation_to_root(
                        &current_root,
                        current_history.as_deref(),
                        &item.operation,
                    )
                    .with_context(|| {
                        format!("semantic_conflict: migration {}", item.migration_hash)
                    })?;
                let postconditions = self.postconditions_for(&produced, &item.operation);
                let failed_postconditions =
                    self.failed_postconditions(&produced, &postconditions)?;
                if !failed_postconditions.is_empty() {
                    bail!(
                        "semantic_conflict: migration {} failed postconditions {}",
                        item.migration_hash,
                        condition_names(&failed_postconditions)
                    );
                }
                produced
            } else {
                let postconditions = self.postconditions_for(&current_root, &item.operation);
                let failed_postconditions =
                    self.failed_postconditions(&current_root, &postconditions)?;
                if failed_postconditions.is_empty() {
                    current_root.clone()
                } else {
                    bail!(
                        "semantic_conflict: migration {} failed preconditions {}",
                        item.migration_hash,
                        condition_names(&failed_preconditions)
                    );
                }
            };
            if produced != item.output_root {
                bail!(
                    "replay mismatch for {}: expected {}, produced {}",
                    item.migration_hash,
                    item.output_root,
                    produced
                );
            }
            let recomputed_history =
                history_hash(current_history.as_deref(), &item.migration_hash, &produced);
            if recomputed_history != item.history_hash {
                bail!(
                    "bad_history_link: expected history {}, recomputed {}",
                    item.history_hash,
                    recomputed_history
                );
            }
            current_root = produced;
            current_history = Some(recomputed_history);
        }

        if current_root != expected.root_hash {
            bail!(
                "replay final root mismatch: expected {}, replayed {}",
                expected.root_hash,
                current_root
            );
        }
        if current_history != expected.history_hash {
            bail!(
                "replay final history mismatch: expected {:?}, replayed {:?}",
                expected.history_hash,
                current_history
            );
        }
        Ok(format!(
            "replay ok\nroot {}\nhistory {}\n",
            current_root,
            current_history.unwrap_or_else(|| "none".to_string())
        ))
    }

    fn history_chain(&self, branch: &str) -> Result<Vec<HistoryItem>> {
        let state = self.branch(branch)?;
        let mut items = Vec::new();
        let mut cursor = state.history_hash;
        while let Some(history_hash_value) = cursor {
            let (parent_history, migration_hash_value, output_root): (
                Option<String>,
                String,
                String,
            ) = self.conn.query_row(
                "SELECT parent_history_hash, migration_hash, output_root_hash
                 FROM histories WHERE history_hash = ?1",
                params![history_hash_value],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let (input_root, operation_kind, operation_json): (String, String, String) =
                self.conn.query_row(
                    "SELECT input_root_hash, operation_kind, operation_json
                     FROM migrations WHERE hash = ?1",
                    params![migration_hash_value],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
            let operation: Operation = serde_json::from_str(&operation_json)?;
            items.push(HistoryItem {
                history_hash: history_hash_value,
                migration_hash: migration_hash_value,
                input_root,
                output_root,
                operation_kind,
                operation,
            });
            cursor = parent_history;
        }
        items.reverse();
        Ok(items)
    }

    fn recorded_operation_output_matches(
        &self,
        input_root: &str,
        output_root: &str,
        op: &Operation,
    ) -> Result<bool> {
        let operation_json = canonical_json(&serde_json::to_value(op)?);
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM migrations
                WHERE input_root_hash = ?1
                  AND output_root_hash = ?2
                  AND operation_kind = ?3
                  AND operation_json = ?4
            )",
            params![input_root, output_root, op.kind_name(), operation_json],
            |row| row.get(0),
        )?)
    }

    fn recorded_operation_exists(&self, op: &Operation) -> Result<bool> {
        let operation_json = canonical_json(&serde_json::to_value(op)?);
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM migrations
                WHERE operation_kind = ?1
                  AND operation_json = ?2
            )",
            params![op.kind_name(), operation_json],
            |row| row.get(0),
        )?)
    }
}

#[derive(Debug)]
struct HistoryItem {
    history_hash: String,
    migration_hash: String,
    input_root: String,
    output_root: String,
    operation_kind: String,
    operation: Operation,
}

fn name_points_to_symbol(
    root: &ProgramRootPayload,
    module: &str,
    name: &str,
    symbol: &str,
) -> bool {
    root.names.iter().any(|binding| {
        binding.module == module && binding.display_name == name && binding.symbol == symbol
    })
}

fn preferred_name_points_to_symbol(
    root: &ProgramRootPayload,
    module: &str,
    name: &str,
    symbol: &str,
) -> bool {
    root.names.iter().any(|binding| {
        binding.module == module
            && binding.display_name == name
            && binding.symbol == symbol
            && binding.is_preferred
    })
}

fn alias_points_to_symbol(
    root: &ProgramRootPayload,
    module: &str,
    alias: &str,
    symbol: &str,
) -> bool {
    root.names.iter().any(|binding| {
        binding.module == module
            && binding.display_name == alias
            && binding.symbol == symbol
            && !binding.is_preferred
    })
}

fn symbol_for_name(root: &ProgramRootPayload, module: &str, name: &str) -> Option<String> {
    root.names
        .iter()
        .find(|binding| binding.module == module && binding.display_name == name)
        .map(|binding| binding.symbol.clone())
}

fn export_points_to_symbol(root: &ProgramRootPayload, name: &str, symbol: &str) -> bool {
    root.exports
        .iter()
        .any(|binding| binding.exported_name == name && binding.symbol == symbol)
}

fn validate_param_names(params: &[ParamSpec]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for param in params {
        validate_projection_identifier("parameter name", &param.name)?;
        if !seen.insert(param.name.clone()) {
            bail!("duplicate parameter name {:?}", param.name);
        }
    }
    Ok(())
}

fn normalize_param_refs(expr: &RawExpr, local_params: &[String]) -> RawExpr {
    match expr {
        RawExpr::LiteralI64 { value } => RawExpr::LiteralI64 {
            value: value.clone(),
        },
        RawExpr::LiteralBool { value } => RawExpr::LiteralBool { value: *value },
        RawExpr::ParamRef { index } => RawExpr::ParamRef { index: *index },
        RawExpr::ParamName { name } => local_params
            .iter()
            .position(|candidate| candidate == name)
            .map(|index| RawExpr::ParamRef { index })
            .unwrap_or_else(|| RawExpr::ParamName { name: name.clone() }),
        RawExpr::Call { name, args } => RawExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| normalize_param_refs(arg, local_params))
                .collect(),
        },
        RawExpr::Binary { op, left, right } => RawExpr::Binary {
            op: op.clone(),
            left: Box::new(normalize_param_refs(left, local_params)),
            right: Box::new(normalize_param_refs(right, local_params)),
        },
        RawExpr::If {
            cond,
            then_expr,
            else_expr,
        } => RawExpr::If {
            cond: Box::new(normalize_param_refs(cond, local_params)),
            then_expr: Box::new(normalize_param_refs(then_expr, local_params)),
            else_expr: Box::new(normalize_param_refs(else_expr, local_params)),
        },
    }
}

pub(crate) fn migration_hash(
    parent_history_hash: Option<&str>,
    input_root_hash: &str,
    output_root_hash: &str,
    operation: &JsonValue,
    preconditions: &JsonValue,
    postconditions: &JsonValue,
) -> String {
    let payload = json!({
        "parent_history_hash": parent_history_hash,
        "input_root_hash": input_root_hash,
        "output_root_hash": output_root_hash,
        "operation": operation,
        "preconditions": preconditions,
        "postconditions": postconditions,
    });
    hash_bytes(MIGRATION_DOMAIN, canonical_json(&payload).as_bytes())
}

pub(crate) fn history_hash(
    parent_history_hash: Option<&str>,
    migration_hash: &str,
    output_root_hash: &str,
) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(parent_history_hash.unwrap_or("").as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(migration_hash.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(output_root_hash.as_bytes());
    hash_bytes(HISTORY_DOMAIN, &bytes)
}
