use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::abi::validate_exported_abi_name;
use crate::build_plan::{BuildImpact, BuildImpactKind, BuildImpactReason, projection_artifacts};
use crate::expr::{RawCaseArm, RawExpr, RawRecordField};
use crate::model::{
    BranchState, ExportBinding, NameBinding, ParamNames, ProgramRootPayload, RootSymbolPayload,
    RootTestBinding, RootTypePayload, TestCasePayload, TestCategory, TestMode, TestValue,
    TypeNameBinding, param_names, root_symbol_index, root_type_index, synchronize_module_metadata,
    test_binding_for, upsert_param_names, validate_module_path, validate_projection_identifier,
};
use crate::store::{CodeDb, canonical_json, hash_bytes};
use crate::tests::{test_points_to_entry_symbol, validate_test_value_for_type};
use crate::types::{
    Effect, ParamSpec, RegionParamDef, SymbolBirthSpec, TypeDefinition, TypeDefinitionIdentity,
    TypeDefinitionKind, TypeMemberDef, TypeMemberSpec, TypeSpec, type_hash_for,
};
use crate::{HISTORY_DOMAIN, MAIN_BRANCH, MIGRATION_DOMAIN};

const HISTORY_EXPORT_SCHEMA: &str = "codedb/history-export/v1";
const HISTORY_EXPORT_MIGRATION_SCHEMA: &str = "codedb/history-export-migration/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Operation {
    CreateFunction {
        module: String,
        name: String,
        birth_seed: String,
        #[serde(default)]
        region_params: Vec<String>,
        params: Vec<ParamSpec>,
        return_type: String,
        #[serde(default)]
        effects: Vec<Effect>,
        body: RawExpr,
    },
    CreateExternalFunction {
        module: String,
        name: String,
        birth_seed: String,
        #[serde(default)]
        region_params: Vec<String>,
        params: Vec<ParamSpec>,
        return_type: String,
        #[serde(default)]
        effects: Vec<Effect>,
        abi: String,
        link_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        library: Option<String>,
    },
    CreateType {
        module: String,
        name: String,
        birth_seed: String,
        #[serde(default)]
        region_params: Vec<String>,
        definition: TypeDefinitionKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<TypeDefinitionIdentity>,
    },
    RenameType {
        module: String,
        type_symbol: String,
        old_name: String,
        new_name: String,
    },
    MoveType {
        module: String,
        type_symbol: String,
        name: String,
        new_module: String,
    },
    AddField {
        module: String,
        type_symbol: String,
        type_name: String,
        field: TypeMemberSpec,
        field_birth_seed: String,
    },
    RenameField {
        module: String,
        type_symbol: String,
        type_name: String,
        field_symbol: String,
        old_name: String,
        new_name: String,
    },
    RemoveField {
        module: String,
        type_symbol: String,
        type_name: String,
        field_symbol: String,
        name: String,
    },
    AddVariant {
        module: String,
        type_symbol: String,
        type_name: String,
        variant: TypeMemberSpec,
        variant_birth_seed: String,
    },
    RenameVariant {
        module: String,
        type_symbol: String,
        type_name: String,
        variant_symbol: String,
        old_name: String,
        new_name: String,
    },
    RemoveVariant {
        module: String,
        type_symbol: String,
        type_name: String,
        variant_symbol: String,
        name: String,
    },
    RenameSymbol {
        module: String,
        symbol: String,
        old_name: String,
        new_name: String,
    },
    MoveSymbol {
        module: String,
        symbol: String,
        name: String,
        new_module: String,
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
        #[serde(default)]
        region_params: Vec<String>,
        params: Vec<ParamSpec>,
        return_type: String,
        #[serde(default)]
        effects: Vec<Effect>,
    },
    AddParameter {
        module: String,
        symbol: String,
        name: String,
        param: ParamSpec,
        default: Option<RawExpr>,
    },
    ConvertParamToReference {
        module: String,
        symbol: String,
        name: String,
        param_index: usize,
        param_name: String,
        region: String,
        #[serde(default)]
        mutable: bool,
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
    CreateTest {
        name: String,
        entry_module: String,
        entry_name: String,
        entry_symbol: String,
        #[serde(default, skip_serializing_if = "TestCategory::is_behavior")]
        category: TestCategory,
        #[serde(default, skip_serializing_if = "TestMode::is_reference")]
        mode: TestMode,
        #[serde(default)]
        args: Vec<TestValue>,
        expected: TestValue,
        #[serde(default)]
        native_agreement: bool,
        #[serde(default)]
        native_required: bool,
    },
    DeleteTest {
        name: String,
        test: String,
    },
    MergeBranch {
        target_branch: String,
        source_branch: String,
        ancestor_root_hash: String,
        ancestor_history_hash: Option<String>,
        source_root_hash: String,
        source_history_hash: Option<String>,
        merged_root: ProgramRootPayload,
        object_payloads: Vec<MergeObjectPayload>,
    },
}

impl Operation {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Operation::CreateFunction { .. } => "create_function",
            Operation::CreateExternalFunction { .. } => "create_external_function",
            Operation::CreateType { .. } => "create_type",
            Operation::RenameType { .. } => "rename_type",
            Operation::MoveType { .. } => "move_type",
            Operation::AddField { .. } => "add_field",
            Operation::RenameField { .. } => "rename_field",
            Operation::RemoveField { .. } => "remove_field",
            Operation::AddVariant { .. } => "add_variant",
            Operation::RenameVariant { .. } => "rename_variant",
            Operation::RemoveVariant { .. } => "remove_variant",
            Operation::RenameSymbol { .. } => "rename_symbol",
            Operation::MoveSymbol { .. } => "move_symbol",
            Operation::ReplaceFunctionBody { .. } => "replace_function_body",
            Operation::ChangeFunctionSignature { .. } => "change_function_signature",
            Operation::AddParameter { .. } => "add_parameter",
            Operation::ConvertParamToReference { .. } => "convert_param_to_reference",
            Operation::DeleteSymbol { .. } => "delete_symbol",
            Operation::CreateAlias { .. } => "create_alias",
            Operation::RemoveAlias { .. } => "remove_alias",
            Operation::SetExport { .. } => "set_export",
            Operation::RemoveExport { .. } => "remove_export",
            Operation::CreateTest { .. } => "create_test",
            Operation::DeleteTest { .. } => "delete_test",
            Operation::MergeBranch { .. } => "merge_branch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MergeObjectPayload {
    pub(crate) hash: String,
    pub(crate) kind: String,
    pub(crate) payload: JsonValue,
}

struct ChangeSignatureApply<'a> {
    input_root: &'a str,
    module: &'a str,
    symbol: &'a str,
    name: &'a str,
    region_params: &'a [String],
    params: &'a [ParamSpec],
    return_type: &'a str,
    effects: &'a [Effect],
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

    pub(crate) fn to_json(&self) -> JsonValue {
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
    ExternalFunctionCreated,
    TypeCreated,
    TypeRenamed,
    TypeMoved,
    TypeDefinitionChanged,
    SymbolRenamed,
    SymbolMoved,
    ImplementationChanged,
    InterfaceChanged,
    SymbolDeleted,
    AliasCreated,
    AliasRemoved,
    ExportSet,
    ExportRemoved,
    TestCreated,
    TestDeleted,
    BranchMerged,
}

impl SemanticImpact {
    fn as_str(self) -> &'static str {
        match self {
            SemanticImpact::FunctionCreated => "function_created",
            SemanticImpact::ExternalFunctionCreated => "external_function_created",
            SemanticImpact::TypeCreated => "type_created",
            SemanticImpact::TypeRenamed => "type_renamed",
            SemanticImpact::TypeMoved => "type_moved",
            SemanticImpact::TypeDefinitionChanged => "type_definition_changed",
            SemanticImpact::SymbolRenamed => "symbol_renamed",
            SemanticImpact::SymbolMoved => "symbol_moved",
            SemanticImpact::ImplementationChanged => "implementation_changed",
            SemanticImpact::InterfaceChanged => "interface_changed",
            SemanticImpact::SymbolDeleted => "symbol_deleted",
            SemanticImpact::AliasCreated => "alias_created",
            SemanticImpact::AliasRemoved => "alias_removed",
            SemanticImpact::ExportSet => "export_set",
            SemanticImpact::ExportRemoved => "export_removed",
            SemanticImpact::TestCreated => "test_created",
            SemanticImpact::TestDeleted => "test_deleted",
            SemanticImpact::BranchMerged => "branch_merged",
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
    TypeNameIsAvailable {
        module: String,
        name: String,
    },
    NamePointsToSymbol {
        module: String,
        name: String,
        symbol: String,
    },
    TypeNamePointsToType {
        module: String,
        name: String,
        type_symbol: String,
    },
    PreferredTypeNamePointsToType {
        module: String,
        name: String,
        type_symbol: String,
    },
    FieldPointsToSymbol {
        type_symbol: String,
        name: String,
        field_symbol: String,
    },
    FieldNameIsAvailable {
        type_symbol: String,
        name: String,
    },
    VariantPointsToSymbol {
        type_symbol: String,
        name: String,
        variant_symbol: String,
    },
    VariantNameIsAvailable {
        type_symbol: String,
        name: String,
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
    TestNameIsAvailable {
        name: String,
    },
    TestNamePointsToTest {
        name: String,
        test: String,
    },
}

impl Precondition {
    fn kind_name(&self) -> &'static str {
        match self {
            Precondition::RootIsCurrent { .. } => "root_is_current",
            Precondition::NameIsAvailable { .. } => "name_is_available",
            Precondition::TypeNameIsAvailable { .. } => "type_name_is_available",
            Precondition::NamePointsToSymbol { .. } => "name_points_to_symbol",
            Precondition::TypeNamePointsToType { .. } => "type_name_points_to_type",
            Precondition::PreferredTypeNamePointsToType { .. } => {
                "preferred_type_name_points_to_type"
            }
            Precondition::FieldPointsToSymbol { .. } => "field_points_to_symbol",
            Precondition::FieldNameIsAvailable { .. } => "field_name_is_available",
            Precondition::VariantPointsToSymbol { .. } => "variant_points_to_symbol",
            Precondition::VariantNameIsAvailable { .. } => "variant_name_is_available",
            Precondition::PreferredNamePointsToSymbol { .. } => "preferred_name_points_to_symbol",
            Precondition::AliasPointsToSymbol { .. } => "alias_points_to_symbol",
            Precondition::ExportNameIsAvailable { .. } => "export_name_is_available",
            Precondition::ExportPointsToSymbol { .. } => "export_points_to_symbol",
            Precondition::TestNameIsAvailable { .. } => "test_name_is_available",
            Precondition::TestNamePointsToTest { .. } => "test_name_points_to_test",
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
        region_params: Vec<String>,
        params: Vec<ParamSpec>,
        return_type: String,
        effects: Vec<Effect>,
        body: RawExpr,
    },
    ExternalFunctionSourceMatches {
        module: String,
        name: String,
        region_params: Vec<String>,
        params: Vec<ParamSpec>,
        return_type: String,
        effects: Vec<Effect>,
        abi: String,
        link_name: String,
        library: Option<String>,
    },
    TypeSourceMatches {
        module: String,
        name: String,
        region_params: Vec<String>,
        definition: TypeDefinitionKind,
    },
    NamePointsToSymbol {
        module: String,
        name: String,
        symbol: String,
    },
    TypeNamePointsToType {
        module: String,
        name: String,
        type_symbol: String,
    },
    NameAbsent {
        module: String,
        name: String,
    },
    TypeNameAbsent {
        module: String,
        name: String,
    },
    FieldPointsToSymbol {
        type_symbol: String,
        name: String,
        field_symbol: String,
    },
    FieldAbsent {
        type_symbol: String,
        name: String,
        field_symbol: String,
    },
    VariantPointsToSymbol {
        type_symbol: String,
        name: String,
        variant_symbol: String,
    },
    VariantAbsent {
        type_symbol: String,
        name: String,
        variant_symbol: String,
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
        region_params: Vec<String>,
        params: Vec<ParamSpec>,
        return_type: String,
        effects: Vec<Effect>,
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
    TestNamePointsToTest {
        name: String,
        test: String,
    },
    TestAbsent {
        name: String,
        test: String,
    },
}

impl Postcondition {
    fn kind_name(&self) -> &'static str {
        match self {
            Postcondition::RootExists { .. } => "root_exists",
            Postcondition::FunctionSourceMatches { .. } => "function_source_matches",
            Postcondition::ExternalFunctionSourceMatches { .. } => {
                "external_function_source_matches"
            }
            Postcondition::TypeSourceMatches { .. } => "type_source_matches",
            Postcondition::NamePointsToSymbol { .. } => "name_points_to_symbol",
            Postcondition::TypeNamePointsToType { .. } => "type_name_points_to_type",
            Postcondition::NameAbsent { .. } => "name_absent",
            Postcondition::TypeNameAbsent { .. } => "type_name_absent",
            Postcondition::FieldPointsToSymbol { .. } => "field_points_to_symbol",
            Postcondition::FieldAbsent { .. } => "field_absent",
            Postcondition::VariantPointsToSymbol { .. } => "variant_points_to_symbol",
            Postcondition::VariantAbsent { .. } => "variant_absent",
            Postcondition::BodySourceMatches { .. } => "body_source_matches",
            Postcondition::SignatureSourceMatches { .. } => "signature_source_matches",
            Postcondition::SymbolAbsent { .. } => "symbol_absent",
            Postcondition::ExportPointsToSymbol { .. } => "export_points_to_symbol",
            Postcondition::ExportAbsent { .. } => "export_absent",
            Postcondition::TestNamePointsToTest { .. } => "test_name_points_to_test",
            Postcondition::TestAbsent { .. } => "test_absent",
        }
    }
}

enum MemberRename {
    Field {
        type_symbol: String,
        member_symbol: String,
        old_name: String,
        new_name: String,
    },
    Variant {
        type_symbol: String,
        member_symbol: String,
        old_name: String,
        new_name: String,
    },
}

impl MemberRename {
    fn new_name(&self) -> &str {
        match self {
            MemberRename::Field { new_name, .. } | MemberRename::Variant { new_name, .. } => {
                new_name
            }
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

fn append_default_arg_to_calls(expr: &RawExpr, target_name: &str, default: &RawExpr) -> RawExpr {
    match expr {
        RawExpr::LiteralI64 { value } => RawExpr::LiteralI64 {
            value: value.clone(),
        },
        RawExpr::LiteralBool { value } => RawExpr::LiteralBool { value: *value },
        RawExpr::LiteralString { value } => RawExpr::LiteralString {
            value: value.clone(),
        },
        RawExpr::LiteralBytes { bytes_hex } => RawExpr::LiteralBytes {
            bytes_hex: bytes_hex.clone(),
        },
        RawExpr::Unit => RawExpr::Unit,
        RawExpr::ParamRef { index } => RawExpr::ParamRef { index: *index },
        RawExpr::ParamName { name } => RawExpr::ParamName { name: name.clone() },
        RawExpr::Call { name, args } => {
            let mut args = args
                .iter()
                .map(|arg| append_default_arg_to_calls(arg, target_name, default))
                .collect::<Vec<_>>();
            if name == target_name {
                args.push(default.clone());
            }
            RawExpr::Call {
                name: name.clone(),
                args,
            }
        }
        RawExpr::Binary { op, left, right } => RawExpr::Binary {
            op: op.clone(),
            left: Box::new(append_default_arg_to_calls(left, target_name, default)),
            right: Box::new(append_default_arg_to_calls(right, target_name, default)),
        },
        RawExpr::Unary { op, expr } => RawExpr::Unary {
            op: op.clone(),
            expr: Box::new(append_default_arg_to_calls(expr, target_name, default)),
        },
        RawExpr::BorrowShared { region, target } => RawExpr::BorrowShared {
            region: region.clone(),
            target: Box::new(append_default_arg_to_calls(target, target_name, default)),
        },
        RawExpr::BorrowMut { region, target } => RawExpr::BorrowMut {
            region: region.clone(),
            target: Box::new(append_default_arg_to_calls(target, target_name, default)),
        },
        RawExpr::Assign { target, value } => RawExpr::Assign {
            target: Box::new(append_default_arg_to_calls(target, target_name, default)),
            value: Box::new(append_default_arg_to_calls(value, target_name, default)),
        },
        RawExpr::Let {
            name,
            ty,
            value,
            body,
        } => RawExpr::Let {
            name: name.clone(),
            ty: ty.clone(),
            value: Box::new(append_default_arg_to_calls(value, target_name, default)),
            body: Box::new(append_default_arg_to_calls(body, target_name, default)),
        },
        RawExpr::If {
            cond,
            then_expr,
            else_expr,
        } => RawExpr::If {
            cond: Box::new(append_default_arg_to_calls(cond, target_name, default)),
            then_expr: Box::new(append_default_arg_to_calls(then_expr, target_name, default)),
            else_expr: Box::new(append_default_arg_to_calls(else_expr, target_name, default)),
        },
        RawExpr::Fold {
            item,
            target,
            acc,
            init,
            body,
        } => RawExpr::Fold {
            item: item.clone(),
            target: Box::new(append_default_arg_to_calls(target, target_name, default)),
            acc: acc.clone(),
            init: Box::new(append_default_arg_to_calls(init, target_name, default)),
            body: Box::new(append_default_arg_to_calls(body, target_name, default)),
        },
        RawExpr::Array { elements } => RawExpr::Array {
            elements: elements
                .iter()
                .map(|element| append_default_arg_to_calls(element, target_name, default))
                .collect(),
        },
        RawExpr::Index { target, index } => RawExpr::Index {
            target: Box::new(append_default_arg_to_calls(target, target_name, default)),
            index: Box::new(append_default_arg_to_calls(index, target_name, default)),
        },
        RawExpr::Record { fields } => RawExpr::Record {
            fields: fields
                .iter()
                .map(|field| crate::expr::RawRecordField {
                    name: field.name.clone(),
                    value: append_default_arg_to_calls(&field.value, target_name, default),
                })
                .collect(),
        },
        RawExpr::FieldAccess { target, field } => RawExpr::FieldAccess {
            target: Box::new(append_default_arg_to_calls(target, target_name, default)),
            field: field.clone(),
        },
        RawExpr::EnumConstruct {
            enum_type,
            variant,
            value,
        } => RawExpr::EnumConstruct {
            enum_type: enum_type.clone(),
            variant: variant.clone(),
            value: Box::new(append_default_arg_to_calls(value, target_name, default)),
        },
        RawExpr::Case { expr, arms } => RawExpr::Case {
            expr: Box::new(append_default_arg_to_calls(expr, target_name, default)),
            arms: arms
                .iter()
                .map(|arm| crate::expr::RawCaseArm {
                    variant: arm.variant.clone(),
                    default: arm.default,
                    binding: arm.binding.clone(),
                    body: append_default_arg_to_calls(&arm.body, target_name, default),
                })
                .collect(),
        },
    }
}

fn operation_summary_parts(op: &Operation) -> (String, SemanticImpact, TypecheckImpact) {
    match op {
        Operation::CreateFunction { module, name, .. } => (
            format!("{module}.{name}"),
            SemanticImpact::FunctionCreated,
            TypecheckImpact::Checked,
        ),
        Operation::CreateExternalFunction { module, name, .. } => (
            format!("{module}.{name}"),
            SemanticImpact::ExternalFunctionCreated,
            TypecheckImpact::Checked,
        ),
        Operation::CreateType {
            module,
            name,
            definition,
            ..
        } => (
            format!("{} {module}.{name}", definition.kind_name()),
            SemanticImpact::TypeCreated,
            TypecheckImpact::Checked,
        ),
        Operation::RenameType {
            module,
            old_name,
            new_name,
            ..
        } => (
            format!("{module}.{old_name} -> {module}.{new_name}"),
            SemanticImpact::TypeRenamed,
            TypecheckImpact::Unchanged,
        ),
        Operation::MoveType {
            module,
            name,
            new_module,
            ..
        } => (
            format!("{module}.{name} -> {new_module}.{name}"),
            SemanticImpact::TypeMoved,
            TypecheckImpact::Unchanged,
        ),
        Operation::AddField {
            module,
            type_name,
            field,
            ..
        } => (
            format!("{module}.{type_name}.{}", field.name),
            SemanticImpact::TypeDefinitionChanged,
            TypecheckImpact::RootRechecked,
        ),
        Operation::RenameField {
            module,
            type_name,
            old_name,
            new_name,
            ..
        } => (
            format!("{module}.{type_name}.{old_name} -> {new_name}"),
            SemanticImpact::TypeDefinitionChanged,
            TypecheckImpact::RootRechecked,
        ),
        Operation::RemoveField {
            module,
            type_name,
            name,
            ..
        } => (
            format!("{module}.{type_name}.{name}"),
            SemanticImpact::TypeDefinitionChanged,
            TypecheckImpact::RootRechecked,
        ),
        Operation::AddVariant {
            module,
            type_name,
            variant,
            ..
        } => (
            format!("{module}.{type_name}.{}", variant.name),
            SemanticImpact::TypeDefinitionChanged,
            TypecheckImpact::RootRechecked,
        ),
        Operation::RenameVariant {
            module,
            type_name,
            old_name,
            new_name,
            ..
        } => (
            format!("{module}.{type_name}.{old_name} -> {new_name}"),
            SemanticImpact::TypeDefinitionChanged,
            TypecheckImpact::RootRechecked,
        ),
        Operation::RemoveVariant {
            module,
            type_name,
            name,
            ..
        } => (
            format!("{module}.{type_name}.{name}"),
            SemanticImpact::TypeDefinitionChanged,
            TypecheckImpact::RootRechecked,
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
        Operation::MoveSymbol {
            module,
            name,
            new_module,
            ..
        } => (
            format!("{module}.{name} -> {new_module}.{name}"),
            SemanticImpact::SymbolMoved,
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
        Operation::AddParameter { module, name, .. } => (
            format!("{module}.{name}"),
            SemanticImpact::InterfaceChanged,
            TypecheckImpact::RootRechecked,
        ),
        Operation::ConvertParamToReference {
            module,
            name,
            param_name,
            mutable,
            ..
        } => (
            format!(
                "{module}.{name}.{param_name} -> {}reference",
                if *mutable { "mutable " } else { "" }
            ),
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
        Operation::CreateTest {
            name,
            entry_module,
            entry_name,
            category: _,
            ..
        } => (
            format!("{name} for {entry_module}.{entry_name}"),
            SemanticImpact::TestCreated,
            TypecheckImpact::Checked,
        ),
        Operation::DeleteTest { name, .. } => (
            name.clone(),
            SemanticImpact::TestDeleted,
            TypecheckImpact::Unchanged,
        ),
        Operation::MergeBranch {
            target_branch,
            source_branch,
            ..
        } => (
            format!("{target_branch} <- {source_branch}"),
            SemanticImpact::BranchMerged,
            TypecheckImpact::RootRechecked,
        ),
    }
}

fn fallback_build_impact(op: &Operation) -> BuildImpact {
    let (kind, recompile_symbols, relink, changed_symbols, reasons) = match op {
        Operation::CreateFunction { .. } | Operation::CreateExternalFunction { .. } => (
            BuildImpactKind::RecompileSymbols,
            vec![],
            true,
            vec![],
            vec![BuildImpactReason::SymbolAdded],
        ),
        Operation::CreateType { .. }
        | Operation::AddField { .. }
        | Operation::RemoveField { .. }
        | Operation::AddVariant { .. }
        | Operation::RemoveVariant { .. }
        | Operation::RenameField { .. }
        | Operation::RenameVariant { .. } => (
            BuildImpactKind::FullRebuild,
            vec![],
            true,
            vec![],
            vec![BuildImpactReason::UnclassifiedRootChange],
        ),
        Operation::RenameType { .. } | Operation::MoveType { .. } => (
            BuildImpactKind::MetadataOnly,
            vec![],
            false,
            vec![],
            vec![BuildImpactReason::MetadataChanged],
        ),
        Operation::RenameSymbol { .. }
        | Operation::MoveSymbol { .. }
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
        Operation::ChangeFunctionSignature { symbol, .. }
        | Operation::AddParameter { symbol, .. } => (
            BuildImpactKind::RecompileDependents,
            vec![symbol.clone()],
            true,
            vec![symbol.clone()],
            vec![BuildImpactReason::InterfaceHashChanged],
        ),
        Operation::ConvertParamToReference { symbol, .. } => (
            BuildImpactKind::RecompileDependents,
            vec![symbol.clone()],
            true,
            vec![symbol.clone()],
            vec![
                BuildImpactReason::InterfaceHashChanged,
                BuildImpactReason::BodyExpressionHashChanged,
                BuildImpactReason::DependencySetChanged,
            ],
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
        Operation::CreateTest { .. } | Operation::DeleteTest { .. } => (
            BuildImpactKind::MetadataOnly,
            vec![],
            false,
            vec![],
            vec![BuildImpactReason::MetadataChanged],
        ),
        Operation::MergeBranch { .. } => (
            BuildImpactKind::FullRebuild,
            vec![],
            true,
            vec![],
            vec![BuildImpactReason::UnclassifiedRootChange],
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
        let result = self.apply_and_record_expected_in_tx_on_branch(MAIN_BRANCH, expected_root, op);

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

    pub(crate) fn apply_and_record_expected_in_tx_on_branch(
        &mut self,
        branch_name: &str,
        expected_root: &str,
        op: Operation,
    ) -> Result<(MigrationOutcome, bool)> {
        self.apply_and_record_expected_in_tx_on_branch_with_agent(
            branch_name,
            expected_root,
            op,
            None,
        )
    }

    pub(crate) fn apply_and_record_expected_in_tx_on_branch_with_agent(
        &mut self,
        branch_name: &str,
        expected_root: &str,
        op: Operation,
        agent: Option<&JsonValue>,
    ) -> Result<(MigrationOutcome, bool)> {
        let fallback_summary = self.migration_summary(&op);
        let branch = self.branch(branch_name)?;
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
        let agent_json = agent.cloned().unwrap_or_else(|| json!({}));

        self.conn.execute(
            "INSERT OR IGNORE INTO migrations
             (hash, parent_history_hash, input_root_hash, output_root_hash,
              operation_kind, operation_json, preconditions_json, postconditions_json, agent_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                migration_hash,
                branch.history_hash,
                old_root,
                new_root,
                op.kind_name(),
                canonical_json(&operation_json),
                canonical_json(&preconditions_json),
                canonical_json(&postconditions_json),
                canonical_json(&agent_json),
            ],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO histories
             (history_hash, parent_history_hash, migration_hash, output_root_hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![history_hash, branch.history_hash, migration_hash, new_root],
        )?;
        self.update_branch(branch_name, &new_root, &history_hash)?;
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

    pub(crate) fn recorded_operation_sequence_outputs_root(
        &self,
        expected_root: &str,
        actual_root: &str,
        operations: &[Operation],
    ) -> Result<bool> {
        if operations.is_empty() {
            return Ok(expected_root == actual_root);
        }
        let mut current_roots = BTreeSet::from([expected_root.to_string()]);
        for operation in operations {
            let operation_json = canonical_json(&serde_json::to_value(operation)?);
            let mut next_roots = BTreeSet::new();
            for root in &current_roots {
                let mut stmt = self.conn.prepare(
                    "SELECT output_root_hash
                     FROM migrations
                     WHERE input_root_hash = ?1
                       AND operation_kind = ?2
                       AND operation_json = ?3
                     ORDER BY output_root_hash",
                )?;
                for row in stmt.query_map(
                    params![root, operation.kind_name(), operation_json],
                    |row| row.get::<_, String>(0),
                )? {
                    next_roots.insert(row?);
                }
            }
            if next_roots.is_empty() {
                return Ok(false);
            }
            current_roots = next_roots;
        }
        Ok(current_roots.contains(actual_root))
    }

    pub(crate) fn preconditions_for(&self, input_root: &str, op: &Operation) -> Vec<Precondition> {
        match op {
            Operation::CreateFunction { module, name, .. }
            | Operation::CreateExternalFunction { module, name, .. } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::NameIsAvailable {
                    module: module.clone(),
                    name: name.clone(),
                },
            ],
            Operation::CreateType { module, name, .. } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TypeNameIsAvailable {
                    module: module.clone(),
                    name: name.clone(),
                },
            ],
            Operation::RenameType {
                module,
                type_symbol,
                old_name,
                new_name,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::PreferredTypeNamePointsToType {
                    module: module.clone(),
                    name: old_name.clone(),
                    type_symbol: type_symbol.clone(),
                },
                Precondition::TypeNameIsAvailable {
                    module: module.clone(),
                    name: new_name.clone(),
                },
            ],
            Operation::MoveType {
                module,
                type_symbol,
                name,
                new_module,
            } => {
                let mut preconditions = vec![
                    Precondition::RootIsCurrent {
                        root: input_root.to_string(),
                    },
                    Precondition::PreferredTypeNamePointsToType {
                        module: module.clone(),
                        name: name.clone(),
                        type_symbol: type_symbol.clone(),
                    },
                ];
                if module == new_module {
                    return preconditions;
                }
                if let Ok(root) = self.load_root(input_root) {
                    for moved_name in root
                        .type_names
                        .iter()
                        .filter(|binding| {
                            binding.module == *module && binding.type_symbol == *type_symbol
                        })
                        .map(|binding| binding.display_name.clone())
                        .collect::<BTreeSet<_>>()
                    {
                        preconditions.push(Precondition::TypeNameIsAvailable {
                            module: new_module.clone(),
                            name: moved_name,
                        });
                    }
                } else {
                    preconditions.push(Precondition::TypeNameIsAvailable {
                        module: new_module.clone(),
                        name: name.clone(),
                    });
                }
                preconditions
            }
            Operation::AddField {
                module,
                type_symbol,
                type_name,
                field,
                ..
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TypeNamePointsToType {
                    module: module.clone(),
                    name: type_name.clone(),
                    type_symbol: type_symbol.clone(),
                },
                Precondition::FieldNameIsAvailable {
                    type_symbol: type_symbol.clone(),
                    name: field.name.clone(),
                },
            ],
            Operation::RenameField {
                module,
                type_symbol,
                type_name,
                field_symbol,
                old_name,
                new_name,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TypeNamePointsToType {
                    module: module.clone(),
                    name: type_name.clone(),
                    type_symbol: type_symbol.clone(),
                },
                Precondition::FieldPointsToSymbol {
                    type_symbol: type_symbol.clone(),
                    name: old_name.clone(),
                    field_symbol: field_symbol.clone(),
                },
                Precondition::FieldNameIsAvailable {
                    type_symbol: type_symbol.clone(),
                    name: new_name.clone(),
                },
            ],
            Operation::RemoveField {
                module,
                type_symbol,
                type_name,
                field_symbol,
                name,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TypeNamePointsToType {
                    module: module.clone(),
                    name: type_name.clone(),
                    type_symbol: type_symbol.clone(),
                },
                Precondition::FieldPointsToSymbol {
                    type_symbol: type_symbol.clone(),
                    name: name.clone(),
                    field_symbol: field_symbol.clone(),
                },
            ],
            Operation::AddVariant {
                module,
                type_symbol,
                type_name,
                variant,
                ..
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TypeNamePointsToType {
                    module: module.clone(),
                    name: type_name.clone(),
                    type_symbol: type_symbol.clone(),
                },
                Precondition::VariantNameIsAvailable {
                    type_symbol: type_symbol.clone(),
                    name: variant.name.clone(),
                },
            ],
            Operation::RenameVariant {
                module,
                type_symbol,
                type_name,
                variant_symbol,
                old_name,
                new_name,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TypeNamePointsToType {
                    module: module.clone(),
                    name: type_name.clone(),
                    type_symbol: type_symbol.clone(),
                },
                Precondition::VariantPointsToSymbol {
                    type_symbol: type_symbol.clone(),
                    name: old_name.clone(),
                    variant_symbol: variant_symbol.clone(),
                },
                Precondition::VariantNameIsAvailable {
                    type_symbol: type_symbol.clone(),
                    name: new_name.clone(),
                },
            ],
            Operation::RemoveVariant {
                module,
                type_symbol,
                type_name,
                variant_symbol,
                name,
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TypeNamePointsToType {
                    module: module.clone(),
                    name: type_name.clone(),
                    type_symbol: type_symbol.clone(),
                },
                Precondition::VariantPointsToSymbol {
                    type_symbol: type_symbol.clone(),
                    name: name.clone(),
                    variant_symbol: variant_symbol.clone(),
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
            Operation::MoveSymbol {
                module,
                symbol,
                name,
                new_module,
            } => {
                let mut preconditions = vec![
                    Precondition::RootIsCurrent {
                        root: input_root.to_string(),
                    },
                    Precondition::PreferredNamePointsToSymbol {
                        module: module.clone(),
                        name: name.clone(),
                        symbol: symbol.clone(),
                    },
                ];
                if module == new_module {
                    return preconditions;
                }
                if let Ok(root) = self.load_root(input_root) {
                    for moved_name in root
                        .names
                        .iter()
                        .filter(|binding| binding.module == *module && binding.symbol == *symbol)
                        .map(|binding| binding.display_name.clone())
                        .collect::<BTreeSet<_>>()
                    {
                        preconditions.push(Precondition::NameIsAvailable {
                            module: new_module.clone(),
                            name: moved_name,
                        });
                    }
                } else {
                    preconditions.push(Precondition::NameIsAvailable {
                        module: new_module.clone(),
                        name: name.clone(),
                    });
                }
                preconditions
            }
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
            | Operation::AddParameter {
                module,
                symbol,
                name,
                ..
            }
            | Operation::ConvertParamToReference {
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
            Operation::CreateTest {
                name,
                entry_module,
                entry_name,
                entry_symbol,
                category: _,
                ..
            } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TestNameIsAvailable { name: name.clone() },
                Precondition::NamePointsToSymbol {
                    module: entry_module.clone(),
                    name: entry_name.clone(),
                    symbol: entry_symbol.clone(),
                },
            ],
            Operation::DeleteTest { name, test } => vec![
                Precondition::RootIsCurrent {
                    root: input_root.to_string(),
                },
                Precondition::TestNamePointsToTest {
                    name: name.clone(),
                    test: test.clone(),
                },
            ],
            Operation::MergeBranch { .. } => vec![Precondition::RootIsCurrent {
                root: input_root.to_string(),
            }],
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
                region_params,
                params,
                return_type,
                effects,
                body,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::FunctionSourceMatches {
                    module: module.clone(),
                    name: name.clone(),
                    region_params: region_params.clone(),
                    params: params.clone(),
                    return_type: return_type.clone(),
                    effects: effects.clone(),
                    body: body.clone(),
                },
            ],
            Operation::CreateExternalFunction {
                module,
                name,
                region_params,
                params,
                return_type,
                effects,
                abi,
                link_name,
                library,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::ExternalFunctionSourceMatches {
                    module: module.clone(),
                    name: name.clone(),
                    region_params: region_params.clone(),
                    params: params.clone(),
                    return_type: return_type.clone(),
                    effects: effects.clone(),
                    abi: abi.clone(),
                    link_name: link_name.clone(),
                    library: library.clone(),
                },
            ],
            Operation::CreateType {
                module,
                name,
                region_params,
                definition,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::TypeSourceMatches {
                    module: module.clone(),
                    name: name.clone(),
                    region_params: region_params.clone(),
                    definition: definition.clone(),
                },
            ],
            Operation::RenameType {
                module,
                type_symbol,
                old_name,
                new_name,
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::TypeNamePointsToType {
                    module: module.clone(),
                    name: new_name.clone(),
                    type_symbol: type_symbol.clone(),
                },
                Postcondition::TypeNameAbsent {
                    module: module.clone(),
                    name: old_name.clone(),
                },
            ],
            Operation::MoveType {
                module,
                type_symbol,
                name,
                new_module,
            } => {
                let mut postconditions = vec![
                    Postcondition::RootExists {
                        root: output_root.to_string(),
                    },
                    Postcondition::TypeNamePointsToType {
                        module: new_module.clone(),
                        name: name.clone(),
                        type_symbol: type_symbol.clone(),
                    },
                ];
                // A same-module move is a no-op (the precondition short-circuits
                // it too); asserting the name is absent in its own module would
                // contradict the TypeNamePointsToType postcondition above.
                if module != new_module {
                    postconditions.push(Postcondition::TypeNameAbsent {
                        module: module.clone(),
                        name: name.clone(),
                    });
                }
                postconditions
            }
            Operation::AddField {
                type_symbol, field, ..
            } => {
                let field_symbol = self
                    .load_root(output_root)
                    .ok()
                    .and_then(|root| {
                        self.field_symbol_by_name(&root, type_symbol, &field.name)
                            .ok()
                    })
                    .unwrap_or_default();
                vec![
                    Postcondition::RootExists {
                        root: output_root.to_string(),
                    },
                    Postcondition::FieldPointsToSymbol {
                        type_symbol: type_symbol.clone(),
                        name: field.name.clone(),
                        field_symbol,
                    },
                ]
            }
            Operation::RenameField {
                type_symbol,
                field_symbol,
                old_name,
                new_name,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::FieldPointsToSymbol {
                    type_symbol: type_symbol.clone(),
                    name: new_name.clone(),
                    field_symbol: field_symbol.clone(),
                },
                Postcondition::FieldAbsent {
                    type_symbol: type_symbol.clone(),
                    name: old_name.clone(),
                    field_symbol: field_symbol.clone(),
                },
            ],
            Operation::RemoveField {
                type_symbol,
                field_symbol,
                name,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::FieldAbsent {
                    type_symbol: type_symbol.clone(),
                    name: name.clone(),
                    field_symbol: field_symbol.clone(),
                },
            ],
            Operation::AddVariant {
                type_symbol,
                variant,
                ..
            } => {
                let variant_symbol = self
                    .load_root(output_root)
                    .ok()
                    .and_then(|root| {
                        self.variant_symbol_by_name(&root, type_symbol, &variant.name)
                            .ok()
                    })
                    .unwrap_or_default();
                vec![
                    Postcondition::RootExists {
                        root: output_root.to_string(),
                    },
                    Postcondition::VariantPointsToSymbol {
                        type_symbol: type_symbol.clone(),
                        name: variant.name.clone(),
                        variant_symbol,
                    },
                ]
            }
            Operation::RenameVariant {
                type_symbol,
                variant_symbol,
                old_name,
                new_name,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::VariantPointsToSymbol {
                    type_symbol: type_symbol.clone(),
                    name: new_name.clone(),
                    variant_symbol: variant_symbol.clone(),
                },
                Postcondition::VariantAbsent {
                    type_symbol: type_symbol.clone(),
                    name: old_name.clone(),
                    variant_symbol: variant_symbol.clone(),
                },
            ],
            Operation::RemoveVariant {
                type_symbol,
                variant_symbol,
                name,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::VariantAbsent {
                    type_symbol: type_symbol.clone(),
                    name: name.clone(),
                    variant_symbol: variant_symbol.clone(),
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
            Operation::MoveSymbol {
                module,
                symbol,
                name,
                new_module,
            } => {
                let mut postconditions = vec![
                    Postcondition::RootExists {
                        root: output_root.to_string(),
                    },
                    Postcondition::NamePointsToSymbol {
                        module: new_module.clone(),
                        name: name.clone(),
                        symbol: symbol.clone(),
                    },
                ];
                // A same-module move is a no-op (the precondition short-circuits
                // it too); asserting the name is absent in its own module would
                // contradict the NamePointsToSymbol postcondition above.
                if module != new_module {
                    postconditions.push(Postcondition::NameAbsent {
                        module: module.clone(),
                        name: name.clone(),
                    });
                }
                postconditions
            }
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
                region_params,
                params,
                return_type,
                effects,
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::SignatureSourceMatches {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
                    region_params: region_params.clone(),
                    params: params.clone(),
                    return_type: return_type.clone(),
                    effects: effects.clone(),
                },
            ],
            Operation::AddParameter {
                module,
                symbol,
                name,
                ..
            }
            | Operation::ConvertParamToReference {
                module,
                symbol,
                name,
                ..
            } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::NamePointsToSymbol {
                    module: module.clone(),
                    name: name.clone(),
                    symbol: symbol.clone(),
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
            Operation::CreateTest { name, .. } => {
                let test = self
                    .load_root(output_root)
                    .ok()
                    .and_then(|root| {
                        test_binding_for(&root, name).map(|binding| binding.test.clone())
                    })
                    .unwrap_or_default();
                vec![
                    Postcondition::RootExists {
                        root: output_root.to_string(),
                    },
                    Postcondition::TestNamePointsToTest {
                        name: name.clone(),
                        test,
                    },
                ]
            }
            Operation::DeleteTest { name, test } => vec![
                Postcondition::RootExists {
                    root: output_root.to_string(),
                },
                Postcondition::TestAbsent {
                    name: name.clone(),
                    test: test.clone(),
                },
            ],
            Operation::MergeBranch { .. } => vec![Postcondition::RootExists {
                root: output_root.to_string(),
            }],
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
                Precondition::TypeNameIsAvailable { module, name } => !root
                    .type_names
                    .iter()
                    .any(|binding| binding.module == *module && binding.display_name == *name),
                Precondition::NamePointsToSymbol {
                    module,
                    name,
                    symbol,
                } => name_points_to_symbol(&root, module, name, symbol),
                Precondition::TypeNamePointsToType {
                    module,
                    name,
                    type_symbol,
                } => type_name_points_to_type(&root, module, name, type_symbol),
                Precondition::PreferredTypeNamePointsToType {
                    module,
                    name,
                    type_symbol,
                } => preferred_type_name_points_to_type(&root, module, name, type_symbol),
                Precondition::FieldPointsToSymbol {
                    type_symbol,
                    name,
                    field_symbol,
                } => self
                    .field_points_to_symbol(&root, type_symbol, name, field_symbol)
                    .unwrap_or(false),
                Precondition::FieldNameIsAvailable { type_symbol, name } => self
                    .field_name_is_available(&root, type_symbol, name)
                    .unwrap_or(false),
                Precondition::VariantPointsToSymbol {
                    type_symbol,
                    name,
                    variant_symbol,
                } => self
                    .variant_points_to_symbol(&root, type_symbol, name, variant_symbol)
                    .unwrap_or(false),
                Precondition::VariantNameIsAvailable { type_symbol, name } => self
                    .variant_name_is_available(&root, type_symbol, name)
                    .unwrap_or(false),
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
                Precondition::TestNameIsAvailable { name } => {
                    !root.tests.iter().any(|binding| binding.name == *name)
                }
                Precondition::TestNamePointsToTest { name, test } => {
                    test_name_points_to_test(&root, name, test)
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
                region_params,
                params,
                return_type,
                effects,
                body,
            } => {
                let Some(symbol) = symbol_for_name(root, module, name) else {
                    return Ok(false);
                };
                let param_names = params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect::<Vec<_>>();
                Ok(self.function_signature_source_matches(
                    root,
                    module,
                    &symbol,
                    region_params,
                    params,
                    return_type,
                    effects,
                )? && self.function_body_source_matches(
                    root,
                    module,
                    &symbol,
                    body,
                    &param_names,
                )?)
            }
            Postcondition::ExternalFunctionSourceMatches {
                module,
                name,
                region_params,
                params,
                return_type,
                effects,
                abi,
                link_name,
                library,
            } => {
                let Some(symbol) = symbol_for_name(root, module, name) else {
                    return Ok(false);
                };
                self.external_function_source_matches(
                    root,
                    module,
                    &symbol,
                    region_params,
                    params,
                    return_type,
                    effects,
                    abi,
                    link_name,
                    library.as_deref(),
                )
            }
            Postcondition::TypeSourceMatches {
                module,
                name,
                region_params,
                definition,
            } => self.type_source_matches(root, module, name, region_params, definition),
            Postcondition::NamePointsToSymbol {
                module,
                name,
                symbol,
            } => Ok(name_points_to_symbol(root, module, name, symbol)),
            Postcondition::TypeNamePointsToType {
                module,
                name,
                type_symbol,
            } => Ok(type_name_points_to_type(root, module, name, type_symbol)),
            Postcondition::NameAbsent { module, name } => Ok(!root
                .names
                .iter()
                .any(|binding| binding.module == *module && binding.display_name == *name)),
            Postcondition::TypeNameAbsent { module, name } => Ok(!root
                .type_names
                .iter()
                .any(|binding| binding.module == *module && binding.display_name == *name)),
            Postcondition::FieldPointsToSymbol {
                type_symbol,
                name,
                field_symbol,
            } => self.field_points_to_symbol(root, type_symbol, name, field_symbol),
            Postcondition::FieldAbsent {
                type_symbol,
                name,
                field_symbol,
            } => Ok(!self.field_points_to_symbol(root, type_symbol, name, field_symbol)?),
            Postcondition::VariantPointsToSymbol {
                type_symbol,
                name,
                variant_symbol,
            } => self.variant_points_to_symbol(root, type_symbol, name, variant_symbol),
            Postcondition::VariantAbsent {
                type_symbol,
                name,
                variant_symbol,
            } => Ok(!self.variant_points_to_symbol(root, type_symbol, name, variant_symbol)?),
            Postcondition::BodySourceMatches {
                module,
                name,
                symbol,
                body,
            } => {
                if !name_points_to_symbol(root, module, name, symbol) {
                    return Ok(false);
                }
                self.function_body_source_matches(
                    root,
                    module,
                    symbol,
                    body,
                    &param_names(root, symbol),
                )
            }
            Postcondition::SignatureSourceMatches {
                module,
                name,
                symbol,
                region_params,
                params,
                return_type,
                effects,
            } => {
                if !name_points_to_symbol(root, module, name, symbol) {
                    return Ok(false);
                }
                self.function_signature_source_matches(
                    root,
                    module,
                    symbol,
                    region_params,
                    params,
                    return_type,
                    effects,
                )
            }
            Postcondition::SymbolAbsent { symbol } => {
                let test_refs_symbol = root
                    .tests
                    .iter()
                    .filter_map(|binding| self.load_test_case(&binding.test).ok())
                    .any(|case| case.entry_symbol == *symbol);
                Ok(!root.symbols.iter().any(|entry| entry.symbol == *symbol)
                    && !root.names.iter().any(|binding| binding.symbol == *symbol)
                    && !root.exports.iter().any(|binding| binding.symbol == *symbol)
                    && !test_refs_symbol)
            }
            Postcondition::ExportPointsToSymbol { name, symbol } => {
                Ok(export_points_to_symbol(root, name, symbol))
            }
            Postcondition::ExportAbsent { name } => Ok(!root
                .exports
                .iter()
                .any(|binding| binding.exported_name == *name)),
            Postcondition::TestNamePointsToTest { name, test } => {
                Ok(test_name_points_to_test(root, name, test))
            }
            Postcondition::TestAbsent { name, test } => Ok(!root
                .tests
                .iter()
                .any(|binding| binding.name == *name || binding.test == *test)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn function_signature_source_matches(
        &self,
        root: &ProgramRootPayload,
        module: &str,
        symbol: &str,
        region_param_names: &[String],
        params: &[ParamSpec],
        return_type: &str,
        effects: &[Effect],
    ) -> Result<bool> {
        let Some(entry) = self.root_symbol(root, symbol) else {
            return Ok(false);
        };
        let (actual_params, actual_return_type) = self.signature_parts(&entry.signature)?;
        let actual_region_params = self.signature_region_params(&entry.signature)?;
        if actual_region_params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>()
            != region_param_names
        {
            return Ok(false);
        }
        let region_scope = region_scope_from_params(&actual_region_params);
        let expected_params = params
            .iter()
            .map(|param| {
                self.type_hash_for_source_in_root_with_regions(
                    module,
                    root,
                    &param.ty,
                    &region_scope,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let expected_return_type = self.type_hash_for_source_in_root_with_regions(
            module,
            root,
            return_type,
            &region_scope,
        )?;
        let expected_effects = crate::types::normalize_effects(effects)?;
        let actual_effects = self.signature_effects(&entry.signature)?;
        let expected_names = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        Ok(actual_params == expected_params
            && actual_return_type == expected_return_type
            && actual_effects == expected_effects
            && param_names(root, symbol) == expected_names)
    }

    #[allow(clippy::too_many_arguments)]
    fn external_function_source_matches(
        &self,
        root: &ProgramRootPayload,
        module: &str,
        symbol: &str,
        region_params: &[String],
        params: &[ParamSpec],
        return_type: &str,
        effects: &[Effect],
        abi: &str,
        link_name: &str,
        library: Option<&str>,
    ) -> Result<bool> {
        let Some(entry) = self.root_symbol(root, symbol) else {
            return Ok(false);
        };
        if !self.function_signature_source_matches(
            root,
            module,
            symbol,
            region_params,
            params,
            return_type,
            effects,
        )? {
            return Ok(false);
        }
        if !self.definition_is_external(&entry.definition)? {
            return Ok(false);
        }
        let external = self.external_function_metadata(&entry.definition)?;
        Ok(external.abi == abi
            && external.link_name == link_name
            && external.library.as_deref() == library)
    }

    fn function_body_source_matches(
        &self,
        root: &ProgramRootPayload,
        module: &str,
        symbol: &str,
        expected_body: &RawExpr,
        local_params: &[String],
    ) -> Result<bool> {
        let Some(entry) = self.root_symbol(root, symbol) else {
            return Ok(false);
        };
        let body = self.function_body_hash(&entry.definition)?;
        let region_names = self
            .signature_region_params(&entry.signature)?
            .into_iter()
            .map(|param| (param.region, param.name))
            .collect::<BTreeMap<_, _>>();
        let actual =
            self.typed_expr_to_raw_in_module_with_regions(&body, root, module, &region_names)?;
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
                region_params,
                params,
                return_type,
                effects,
                body,
            } => self.apply_create_function(
                input_root,
                parent_history_hash,
                module,
                name,
                birth_seed,
                region_params,
                params,
                return_type,
                effects,
                body,
            ),
            Operation::CreateExternalFunction {
                module,
                name,
                birth_seed,
                region_params,
                params,
                return_type,
                effects,
                abi,
                link_name,
                library,
            } => self.apply_create_external_function(
                input_root,
                parent_history_hash,
                module,
                name,
                birth_seed,
                region_params,
                params,
                return_type,
                effects,
                abi,
                link_name,
                library.as_deref(),
            ),
            Operation::CreateType {
                module,
                name,
                birth_seed,
                region_params,
                definition,
                identity,
            } => self.apply_create_type(
                input_root,
                parent_history_hash,
                module,
                name,
                birth_seed,
                region_params,
                definition,
                identity.as_ref(),
            ),
            Operation::RenameType {
                module,
                type_symbol,
                old_name,
                new_name,
            } => self.apply_rename_type(input_root, module, type_symbol, old_name, new_name),
            Operation::MoveType {
                module,
                type_symbol,
                name,
                new_module,
            } => self.apply_move_type(input_root, module, type_symbol, name, new_module),
            Operation::AddField {
                module,
                type_symbol,
                type_name,
                field,
                field_birth_seed,
            } => self.apply_add_field(
                input_root,
                parent_history_hash,
                module,
                type_symbol,
                type_name,
                field,
                field_birth_seed,
            ),
            Operation::RenameField {
                module,
                type_symbol,
                type_name,
                field_symbol,
                old_name,
                new_name,
            } => self.apply_rename_field(
                input_root,
                module,
                type_symbol,
                type_name,
                field_symbol,
                old_name,
                new_name,
            ),
            Operation::RemoveField {
                module,
                type_symbol,
                type_name,
                field_symbol,
                name,
            } => self.apply_remove_field(
                input_root,
                module,
                type_symbol,
                type_name,
                field_symbol,
                name,
            ),
            Operation::AddVariant {
                module,
                type_symbol,
                type_name,
                variant,
                variant_birth_seed,
            } => self.apply_add_variant(
                input_root,
                parent_history_hash,
                module,
                type_symbol,
                type_name,
                variant,
                variant_birth_seed,
            ),
            Operation::RenameVariant {
                module,
                type_symbol,
                type_name,
                variant_symbol,
                old_name,
                new_name,
            } => self.apply_rename_variant(
                input_root,
                module,
                type_symbol,
                type_name,
                variant_symbol,
                old_name,
                new_name,
            ),
            Operation::RemoveVariant {
                module,
                type_symbol,
                type_name,
                variant_symbol,
                name,
            } => self.apply_remove_variant(
                input_root,
                module,
                type_symbol,
                type_name,
                variant_symbol,
                name,
            ),
            Operation::RenameSymbol {
                module,
                symbol,
                old_name,
                new_name,
            } => self.apply_rename_symbol(input_root, module, symbol, old_name, new_name),
            Operation::MoveSymbol {
                module,
                symbol,
                name,
                new_module,
            } => self.apply_move_symbol(input_root, module, symbol, name, new_module),
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
                region_params,
                params,
                return_type,
                effects,
            } => self.apply_change_signature(ChangeSignatureApply {
                input_root,
                module,
                symbol,
                name,
                region_params,
                params,
                return_type,
                effects,
            }),
            Operation::AddParameter {
                module,
                symbol,
                name,
                param,
                default,
            } => {
                self.apply_add_parameter(input_root, module, symbol, name, param, default.as_ref())
            }
            Operation::ConvertParamToReference {
                module,
                symbol,
                name,
                param_index,
                param_name,
                region,
                mutable,
            } => self.apply_convert_param_to_reference(
                input_root,
                module,
                symbol,
                name,
                *param_index,
                param_name,
                region,
                *mutable,
            ),
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
            Operation::CreateTest {
                name,
                entry_module,
                entry_name,
                entry_symbol,
                category,
                mode,
                args,
                expected,
                native_agreement,
                native_required,
            } => self.apply_create_test(
                input_root,
                name,
                entry_module,
                entry_name,
                entry_symbol,
                *category,
                *mode,
                args,
                expected,
                *native_agreement,
                *native_required,
            ),
            Operation::DeleteTest { name, test } => self.apply_delete_test(input_root, name, test),
            Operation::MergeBranch {
                source_root_hash,
                merged_root,
                object_payloads,
                ..
            } => {
                self.apply_merge_branch(input_root, source_root_hash, merged_root, object_payloads)
            }
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
        region_param_names: &[String],
        params: &[ParamSpec],
        return_type: &str,
        effects: &[Effect],
        body: &RawExpr,
    ) -> Result<String> {
        validate_module_path("module", module)?;
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
        validate_region_param_names(region_param_names)?;
        let region_params = self.function_region_params(
            parent_history_hash,
            &symbol,
            birth_seed,
            region_param_names,
        )?;
        let region_scope = region_scope_from_params(&region_params);
        let param_types = params
            .iter()
            .map(|param| {
                self.resolve_type_in_root_with_regions(module, &root, &param.ty, &region_scope)
            })
            .collect::<Result<Vec<_>>>()?;
        let return_type_hash =
            self.resolve_type_in_root_with_regions(module, &root, return_type, &region_scope)?;
        let signature = self.put_signature_with_effects_and_regions(
            &param_types,
            &return_type_hash,
            effects,
            &region_params,
        )?;
        let param_name_list = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        let typed_body = self.type_expr_in_module_with_regions_expecting(
            module,
            body,
            &root,
            &param_name_list,
            &param_types,
            &region_scope,
            Some(&return_type_hash),
        )?;
        if !self.type_assignable_in_root(&root, &typed_body.type_hash, &return_type_hash)? {
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
        if module != MAIN_BRANCH
            || root
                .metadata
                .contains_key(crate::model::ROOT_MODULES_METADATA_KEY)
        {
            synchronize_module_metadata(&mut root);
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_create_external_function(
        &mut self,
        input_root: &str,
        parent_history_hash: Option<&str>,
        module: &str,
        name: &str,
        birth_seed: &str,
        region_param_names: &[String],
        params: &[ParamSpec],
        return_type: &str,
        effects: &[Effect],
        abi: &str,
        link_name: &str,
        library: Option<&str>,
    ) -> Result<String> {
        validate_module_path("module", module)?;
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
        validate_region_param_names(region_param_names)?;
        let region_params = self.function_region_params(
            parent_history_hash,
            &symbol,
            birth_seed,
            region_param_names,
        )?;
        let region_scope = region_scope_from_params(&region_params);
        let param_types = params
            .iter()
            .map(|param| {
                self.resolve_type_in_root_with_regions(module, &root, &param.ty, &region_scope)
            })
            .collect::<Result<Vec<_>>>()?;
        let return_type_hash =
            self.resolve_type_in_root_with_regions(module, &root, return_type, &region_scope)?;
        let signature = self.put_signature_with_effects_and_regions(
            &param_types,
            &return_type_hash,
            effects,
            &region_params,
        )?;
        let definition =
            self.put_external_function(&symbol, &signature, abi, link_name, library)?;
        let param_name_list = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();

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
        if module != MAIN_BRANCH
            || root
                .metadata
                .contains_key(crate::model::ROOT_MODULES_METADATA_KEY)
        {
            synchronize_module_metadata(&mut root);
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_create_type(
        &mut self,
        input_root: &str,
        parent_history_hash: Option<&str>,
        module: &str,
        name: &str,
        birth_seed: &str,
        region_param_names: &[String],
        definition: &TypeDefinitionKind,
        identity: Option<&TypeDefinitionIdentity>,
    ) -> Result<String> {
        validate_module_path("module", module)?;
        validate_projection_identifier("type name", name)?;
        validate_region_param_names(region_param_names)?;
        validate_type_member_specs(definition)?;
        let mut root = self.load_root(input_root)?;
        if root
            .type_names
            .iter()
            .any(|binding| binding.module == module && binding.display_name == name)
        {
            bail!("type name already exists: {module}.{name}");
        }

        let type_symbol = if let Some(identity) = identity {
            self.put_symbol_birth_spec(&identity.type_symbol_birth, "type", None)?
        } else {
            self.put_type_symbol_birth(parent_history_hash, birth_seed)?
        };
        let (region_params, region_scope) = self.create_region_params(
            parent_history_hash,
            &type_symbol,
            birth_seed,
            region_param_names,
            identity.map(|identity| identity.region_param_births.as_slice()),
        )?;
        // Two-phase create so a type's own fields can reference it and its
        // region parameters — reference-based recursive/self-referential types
        // (SPEC_V2 §11). Register a placeholder definition under the new name and
        // symbol BEFORE resolving member types, so a self-reference resolves;
        // then overwrite it with the fully-resolved definition. Only the
        // placeholder's region-parameter arity is consulted during field
        // resolution; its single dummy member exists solely to satisfy the
        // non-empty-members validation and is discarded by the overwrite. The
        // final root is identical to the single-phase result for non-recursive
        // types (the placeholder TypeDef object is dangling and unreferenced), so
        // replay stays deterministic. (Mutual recursion split across two separate
        // create_type operations is still unsupported: each operation
        // type-checks its own output root, and the second type does not yet exist
        // when the first op finishes.)
        let placeholder_member = TypeMemberDef {
            member_symbol: type_symbol.clone(),
            name: "placeholder".to_string(),
            type_hash: type_hash_for("I64"),
        };
        let placeholder = match definition {
            TypeDefinitionKind::Record { .. } => TypeDefinition::Record {
                type_symbol: type_symbol.clone(),
                region_params: region_params.clone(),
                fields: vec![placeholder_member],
            },
            TypeDefinitionKind::Enum { .. } => TypeDefinition::Enum {
                type_symbol: type_symbol.clone(),
                region_params: region_params.clone(),
                variants: vec![placeholder_member],
            },
        };
        let placeholder_def = self.put_type_def(&type_symbol, &placeholder)?;
        root.types.push(RootTypePayload {
            type_symbol: type_symbol.clone(),
            type_def: placeholder_def,
        });
        root.type_names.push(TypeNameBinding {
            module: module.to_string(),
            display_name: name.to_string(),
            type_symbol: type_symbol.clone(),
            is_preferred: true,
        });
        let semantic_definition = self.type_definition_from_source(
            &root,
            module,
            parent_history_hash,
            birth_seed,
            &type_symbol,
            region_params,
            &region_scope,
            definition,
            identity,
        )?;
        self.update_type_definition(&mut root, &type_symbol, semantic_definition)?;
        if module != MAIN_BRANCH
            || root
                .metadata
                .contains_key(crate::model::ROOT_MODULES_METADATA_KEY)
        {
            synchronize_module_metadata(&mut root);
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_rename_type(
        &mut self,
        input_root: &str,
        module: &str,
        type_symbol: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<String> {
        validate_module_path("module", module)?;
        validate_projection_identifier("type name", new_name)?;
        let mut root = self.load_root(input_root)?;
        if root
            .type_names
            .iter()
            .any(|binding| binding.module == module && binding.display_name == new_name)
        {
            bail!("type name already exists: {module}.{new_name}");
        }
        let mut changed = false;
        for binding in &mut root.type_names {
            if binding.module == module
                && binding.display_name == old_name
                && binding.type_symbol == type_symbol
                && binding.is_preferred
            {
                binding.display_name = new_name.to_string();
                changed = true;
            }
        }
        if !changed {
            bail!("precondition failed: {module}.{old_name} does not point to {type_symbol}");
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_move_type(
        &mut self,
        input_root: &str,
        module: &str,
        type_symbol: &str,
        name: &str,
        new_module: &str,
    ) -> Result<String> {
        validate_module_path("module", module)?;
        validate_module_path("new module", new_module)?;
        let mut root = self.load_root(input_root)?;
        if module == new_module {
            self.assert_type_name_points(&root, module, name, type_symbol)?;
            return Ok(input_root.to_string());
        }
        if !root.type_names.iter().any(|binding| {
            binding.module == module
                && binding.display_name == name
                && binding.type_symbol == type_symbol
                && binding.is_preferred
        }) {
            bail!("precondition failed: {module}.{name} does not point to {type_symbol}");
        }

        let moved_names = root
            .type_names
            .iter()
            .filter(|binding| binding.module == module && binding.type_symbol == type_symbol)
            .map(|binding| binding.display_name.clone())
            .collect::<BTreeSet<_>>();
        for moved_name in &moved_names {
            if root.type_names.iter().any(|binding| {
                binding.module == new_module
                    && binding.display_name == *moved_name
                    && binding.type_symbol != type_symbol
            }) {
                bail!("type name already exists: {new_module}.{moved_name}");
            }
        }

        let mut changed = false;
        for binding in &mut root.type_names {
            if binding.module == module && binding.type_symbol == type_symbol {
                binding.module = new_module.to_string();
                changed = true;
            }
        }
        if !changed {
            bail!("precondition failed: {module}.{name} does not point to {type_symbol}");
        }
        synchronize_module_metadata(&mut root);
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_add_field(
        &mut self,
        input_root: &str,
        parent_history_hash: Option<&str>,
        module: &str,
        type_symbol: &str,
        type_name: &str,
        field: &TypeMemberSpec,
        field_birth_seed: &str,
    ) -> Result<String> {
        validate_projection_identifier("record field", &field.name)?;
        let mut root = self.load_root(input_root)?;
        self.assert_type_name_points(&root, module, type_name, type_symbol)?;
        let definition = self.type_definition_for_symbol(&root, type_symbol)?;
        let TypeDefinition::Record {
            region_params,
            mut fields,
            ..
        } = definition
        else {
            bail!("add_field requires record type {module}.{type_name}");
        };
        if fields.iter().any(|candidate| candidate.name == field.name) {
            bail!(
                "record field already exists: {field_name}",
                field_name = field.name
            );
        }
        let field_symbol =
            self.put_record_field_birth(parent_history_hash, type_symbol, field_birth_seed)?;
        let region_scope = region_scope_from_params(&region_params);
        let type_hash =
            self.resolve_type_in_root_with_regions(module, &root, &field.ty, &region_scope)?;
        fields.push(TypeMemberDef {
            member_symbol: field_symbol,
            name: field.name.clone(),
            type_hash,
        });
        self.update_type_definition(
            &mut root,
            type_symbol,
            TypeDefinition::Record {
                type_symbol: type_symbol.to_string(),
                region_params,
                fields,
            },
        )?;
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_rename_field(
        &mut self,
        input_root: &str,
        module: &str,
        type_symbol: &str,
        type_name: &str,
        field_symbol: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<String> {
        validate_projection_identifier("record field", new_name)?;
        let mut root = self.load_root(input_root)?;
        let old_root = root.clone();
        self.assert_type_name_points(&root, module, type_name, type_symbol)?;
        let definition = self.type_definition_for_symbol(&root, type_symbol)?;
        let TypeDefinition::Record {
            region_params,
            mut fields,
            ..
        } = definition
        else {
            bail!("rename_field requires record type {module}.{type_name}");
        };
        if fields.iter().any(|field| field.name == new_name) {
            bail!("record field already exists: {new_name}");
        }
        let mut changed = false;
        for field in &mut fields {
            if field.member_symbol == field_symbol && field.name == old_name {
                field.name = new_name.to_string();
                changed = true;
            }
        }
        if !changed {
            bail!("record field {old_name} does not point to {field_symbol}");
        }
        self.update_type_definition(
            &mut root,
            type_symbol,
            TypeDefinition::Record {
                type_symbol: type_symbol.to_string(),
                region_params,
                fields,
            },
        )?;
        self.rewrite_function_bodies_for_member_rename(
            &old_root,
            &mut root,
            &MemberRename::Field {
                type_symbol: type_symbol.to_string(),
                member_symbol: field_symbol.to_string(),
                old_name: old_name.to_string(),
                new_name: new_name.to_string(),
            },
        )?;
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_remove_field(
        &mut self,
        input_root: &str,
        module: &str,
        type_symbol: &str,
        type_name: &str,
        field_symbol: &str,
        name: &str,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_type_name_points(&root, module, type_name, type_symbol)?;
        let definition = self.type_definition_for_symbol(&root, type_symbol)?;
        let TypeDefinition::Record {
            region_params,
            mut fields,
            ..
        } = definition
        else {
            bail!("remove_field requires record type {module}.{type_name}");
        };
        let original_len = fields.len();
        fields.retain(|field| !(field.member_symbol == field_symbol && field.name == name));
        if fields.len() == original_len {
            bail!("record field {name} does not point to {field_symbol}");
        }
        self.update_type_definition(
            &mut root,
            type_symbol,
            TypeDefinition::Record {
                type_symbol: type_symbol.to_string(),
                region_params,
                fields,
            },
        )?;
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_add_variant(
        &mut self,
        input_root: &str,
        parent_history_hash: Option<&str>,
        module: &str,
        type_symbol: &str,
        type_name: &str,
        variant: &TypeMemberSpec,
        variant_birth_seed: &str,
    ) -> Result<String> {
        validate_projection_identifier("enum variant", &variant.name)?;
        let mut root = self.load_root(input_root)?;
        self.assert_type_name_points(&root, module, type_name, type_symbol)?;
        let definition = self.type_definition_for_symbol(&root, type_symbol)?;
        let TypeDefinition::Enum {
            region_params,
            mut variants,
            ..
        } = definition
        else {
            bail!("add_variant requires enum type {module}.{type_name}");
        };
        if variants
            .iter()
            .any(|candidate| candidate.name == variant.name)
        {
            bail!(
                "enum variant already exists: {variant_name}",
                variant_name = variant.name
            );
        }
        let variant_symbol =
            self.put_enum_variant_birth(parent_history_hash, type_symbol, variant_birth_seed)?;
        let region_scope = region_scope_from_params(&region_params);
        let type_hash =
            self.resolve_type_in_root_with_regions(module, &root, &variant.ty, &region_scope)?;
        variants.push(TypeMemberDef {
            member_symbol: variant_symbol,
            name: variant.name.clone(),
            type_hash,
        });
        self.update_type_definition(
            &mut root,
            type_symbol,
            TypeDefinition::Enum {
                type_symbol: type_symbol.to_string(),
                region_params,
                variants,
            },
        )?;
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_rename_variant(
        &mut self,
        input_root: &str,
        module: &str,
        type_symbol: &str,
        type_name: &str,
        variant_symbol: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<String> {
        validate_projection_identifier("enum variant", new_name)?;
        let mut root = self.load_root(input_root)?;
        let old_root = root.clone();
        self.assert_type_name_points(&root, module, type_name, type_symbol)?;
        let definition = self.type_definition_for_symbol(&root, type_symbol)?;
        let TypeDefinition::Enum {
            region_params,
            mut variants,
            ..
        } = definition
        else {
            bail!("rename_variant requires enum type {module}.{type_name}");
        };
        if variants.iter().any(|variant| variant.name == new_name) {
            bail!("enum variant already exists: {new_name}");
        }
        let mut changed = false;
        for variant in &mut variants {
            if variant.member_symbol == variant_symbol && variant.name == old_name {
                variant.name = new_name.to_string();
                changed = true;
            }
        }
        if !changed {
            bail!("enum variant {old_name} does not point to {variant_symbol}");
        }
        self.update_type_definition(
            &mut root,
            type_symbol,
            TypeDefinition::Enum {
                type_symbol: type_symbol.to_string(),
                region_params,
                variants,
            },
        )?;
        self.rewrite_function_bodies_for_member_rename(
            &old_root,
            &mut root,
            &MemberRename::Variant {
                type_symbol: type_symbol.to_string(),
                member_symbol: variant_symbol.to_string(),
                old_name: old_name.to_string(),
                new_name: new_name.to_string(),
            },
        )?;
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_remove_variant(
        &mut self,
        input_root: &str,
        module: &str,
        type_symbol: &str,
        type_name: &str,
        variant_symbol: &str,
        name: &str,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_type_name_points(&root, module, type_name, type_symbol)?;
        let definition = self.type_definition_for_symbol(&root, type_symbol)?;
        let TypeDefinition::Enum {
            region_params,
            mut variants,
            ..
        } = definition
        else {
            bail!("remove_variant requires enum type {module}.{type_name}");
        };
        let original_len = variants.len();
        variants
            .retain(|variant| !(variant.member_symbol == variant_symbol && variant.name == name));
        if variants.len() == original_len {
            bail!("enum variant {name} does not point to {variant_symbol}");
        }
        self.update_type_definition(
            &mut root,
            type_symbol,
            TypeDefinition::Enum {
                type_symbol: type_symbol.to_string(),
                region_params,
                variants,
            },
        )?;
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
        validate_module_path("module", module)?;
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

    pub(crate) fn apply_move_symbol(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        new_module: &str,
    ) -> Result<String> {
        validate_module_path("module", module)?;
        validate_module_path("new module", new_module)?;
        let mut root = self.load_root(input_root)?;
        if module == new_module {
            self.assert_name_points(&root, module, name, symbol)?;
            return Ok(input_root.to_string());
        }
        if !root.names.iter().any(|binding| {
            binding.module == module
                && binding.display_name == name
                && binding.symbol == symbol
                && binding.is_preferred
        }) {
            bail!("precondition failed: {module}.{name} does not point to {symbol}");
        }

        let moved_names = root
            .names
            .iter()
            .filter(|binding| binding.module == module && binding.symbol == symbol)
            .map(|binding| binding.display_name.clone())
            .collect::<BTreeSet<_>>();
        if moved_names.is_empty() {
            bail!("precondition failed: no names for {symbol} in module {module}");
        }
        for moved_name in &moved_names {
            if root.names.iter().any(|binding| {
                binding.module == new_module
                    && binding.display_name == *moved_name
                    && binding.symbol != symbol
            }) {
                bail!("name already exists: {new_module}.{moved_name}");
            }
        }

        let mut changed = false;
        for binding in &mut root.names {
            if binding.module == module && binding.symbol == symbol {
                binding.module = new_module.to_string();
                changed = true;
            }
        }
        if !changed {
            bail!("precondition failed: {module}.{name} does not point to {symbol}");
        }
        synchronize_module_metadata(&mut root);
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
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
        if self.definition_is_external(&root.symbols[idx].definition)? {
            bail!("cannot replace body for external function {module}.{name}");
        }
        let (param_types, return_type) = self.signature_parts(&signature)?;
        let region_scope = region_scope_from_params(&self.signature_region_params(&signature)?);
        let param_name_list = param_names(&root, symbol);
        let typed_body = self.type_expr_in_module_with_regions_expecting(
            module,
            body,
            &root,
            &param_name_list,
            &param_types,
            &region_scope,
            Some(&return_type),
        )?;
        if !self.type_assignable_in_root(&root, &typed_body.type_hash, &return_type)? {
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

    fn apply_change_signature(&mut self, change: ChangeSignatureApply<'_>) -> Result<String> {
        let ChangeSignatureApply {
            input_root,
            module,
            symbol,
            name,
            region_params: region_param_names,
            params,
            return_type,
            effects,
        } = change;
        validate_param_names(params)?;
        validate_region_param_names(region_param_names)?;
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let idx = root_symbol_index(&root, symbol)?;
        let old_definition = root.symbols[idx].definition.clone();
        let region_params =
            self.function_region_params(None, symbol, "change_signature", region_param_names)?;
        let region_scope = region_scope_from_params(&region_params);
        let param_types = params
            .iter()
            .map(|param| {
                self.resolve_type_in_root_with_regions(module, &root, &param.ty, &region_scope)
            })
            .collect::<Result<Vec<_>>>()?;
        let return_type_hash =
            self.resolve_type_in_root_with_regions(module, &root, return_type, &region_scope)?;
        let signature = self.put_signature_with_effects_and_regions(
            &param_types,
            &return_type_hash,
            effects,
            &region_params,
        )?;
        let param_name_list = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        if self.definition_is_external(&old_definition)? {
            let external = self.external_function_metadata(&old_definition)?;
            let definition = self.put_external_function(
                symbol,
                &signature,
                &external.abi,
                &external.link_name,
                external.library.as_deref(),
            )?;
            root.symbols[idx].signature = signature;
            root.symbols[idx].definition = definition;
            upsert_param_names(&mut root, symbol, param_name_list);
            let new_root = self.put_program_root(&root)?;
            self.index_root(&new_root)?;
            self.type_check_root(&new_root)
                .context("new external signature invalidates existing root")?;
            return Ok(new_root);
        }
        let old_body_hash = self.function_body_hash(&old_definition)?;
        let old_region_names = self
            .signature_region_params(&root.symbols[idx].signature)?
            .into_iter()
            .map(|param| (param.region, param.name))
            .collect::<BTreeMap<_, _>>();
        let raw_body = self.typed_expr_to_raw_in_module_with_regions(
            &old_body_hash,
            &root,
            module,
            &old_region_names,
        )?;
        let typed_body = self.type_expr_in_module_with_regions_expecting(
            module,
            &raw_body,
            &root,
            &param_name_list,
            &param_types,
            &region_scope,
            Some(&return_type_hash),
        )?;
        if !self.type_assignable_in_root(&root, &typed_body.type_hash, &return_type_hash)? {
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

    pub(crate) fn apply_add_parameter(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        param: &ParamSpec,
        default: Option<&RawExpr>,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let callers = self.reverse_dependencies_for_root(&root, symbol)?;
        if !callers.is_empty() && default.is_none() {
            bail!(
                "add_parameter for {module}.{name} requires default when call sites exist: {}",
                callers
                    .iter()
                    .map(|caller| self.symbol_display(&root, caller))
                    .collect::<Result<Vec<_>>>()?
                    .join(", ")
            );
        }

        let idx = root_symbol_index(&root, symbol)?;
        let old_signature = root.symbols[idx].signature.clone();
        let old_definition = root.symbols[idx].definition.clone();
        if self.definition_is_external(&old_definition)? {
            bail!("cannot add parameter to external function {module}.{name}");
        }
        let old_body_hash = self.function_body_hash(&old_definition)?;
        let old_region_names = self
            .signature_region_params(&old_signature)?
            .into_iter()
            .map(|param| (param.region, param.name))
            .collect::<BTreeMap<_, _>>();
        let old_body = self.typed_expr_to_raw_in_module_with_regions(
            &old_body_hash,
            &root,
            module,
            &old_region_names,
        )?;
        let (mut param_types, return_type) = self.signature_parts(&old_signature)?;
        let region_params = self.signature_region_params(&old_signature)?;
        let region_scope = region_scope_from_params(&region_params);
        let effects = self.signature_effects(&old_signature)?;
        let mut param_name_list = param_names(&root, symbol);
        param_types.push(self.resolve_type_in_root_with_regions(
            module,
            &root,
            &param.ty,
            &region_scope,
        )?);
        param_name_list.push(param.name.clone());
        let params = param_types
            .iter()
            .zip(param_name_list.iter())
            .map(|(type_hash, name)| {
                Ok(ParamSpec {
                    name: name.clone(),
                    ty: self.type_name(type_hash)?.to_string(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        validate_param_names(&params)?;

        let signature = self.put_signature_with_effects_and_regions(
            &param_types,
            &return_type,
            &effects,
            &region_params,
        )?;
        root.symbols[idx].signature = signature.clone();
        upsert_param_names(&mut root, symbol, param_name_list.clone());

        let target_name = self.symbol_display_for_module(&root, module, symbol)?;
        let target_body = if callers.iter().any(|caller| caller == symbol) {
            append_default_arg_to_calls(
                &old_body,
                &target_name,
                default.ok_or_else(|| anyhow!("add_parameter missing default"))?,
            )
        } else {
            old_body
        };
        let typed_body = self.type_expr_in_module_with_regions_expecting(
            module,
            &target_body,
            &root,
            &param_name_list,
            &param_types,
            &region_scope,
            Some(&return_type),
        )?;
        if !self.type_assignable_in_root(&root, &typed_body.type_hash, &return_type)? {
            bail!(
                "body type {} does not match new return type {}",
                self.type_name(&typed_body.type_hash)?,
                self.type_name(&return_type)?
            );
        }
        let definition = self.put_function_def(symbol, &signature, &typed_body.expr_hash)?;
        root.symbols[idx].definition = definition;

        if let Some(default) = default {
            for caller in callers {
                if caller == symbol {
                    continue;
                }
                let caller_idx = root_symbol_index(&root, &caller)?;
                let caller_signature = root.symbols[caller_idx].signature.clone();
                let caller_definition = root.symbols[caller_idx].definition.clone();
                let caller_body_hash = self.function_body_hash(&caller_definition)?;
                let caller_module = self
                    .preferred_binding(&root, &caller)
                    .map(|binding| binding.module.clone())
                    .unwrap_or_else(|| MAIN_BRANCH.to_string());
                let caller_region_names = self
                    .signature_region_params(&caller_signature)?
                    .into_iter()
                    .map(|param| (param.region, param.name))
                    .collect::<BTreeMap<_, _>>();
                let caller_body = self.typed_expr_to_raw_in_module_with_regions(
                    &caller_body_hash,
                    &root,
                    &caller_module,
                    &caller_region_names,
                )?;
                let caller_target_name =
                    self.symbol_display_for_module(&root, &caller_module, symbol)?;
                let patched_body =
                    append_default_arg_to_calls(&caller_body, &caller_target_name, default);
                if patched_body == caller_body {
                    continue;
                }
                let (caller_param_types, caller_return_type) =
                    self.signature_parts(&caller_signature)?;
                let caller_region_scope =
                    region_scope_from_params(&self.signature_region_params(&caller_signature)?);
                let caller_param_names = param_names(&root, &caller);
                let typed_caller = self.type_expr_in_module_with_regions(
                    &caller_module,
                    &patched_body,
                    &root,
                    &caller_param_names,
                    &caller_param_types,
                    &caller_region_scope,
                )?;
                if !self.type_assignable_in_root(
                    &root,
                    &typed_caller.type_hash,
                    &caller_return_type,
                )? {
                    bail!(
                        "caller body type {} does not match return type {}",
                        self.type_name(&typed_caller.type_hash)?,
                        self.type_name(&caller_return_type)?
                    );
                }
                let caller_new_definition =
                    self.put_function_def(&caller, &caller_signature, &typed_caller.expr_hash)?;
                root.symbols[caller_idx].definition = caller_new_definition;
            }
        }

        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)
            .context("added parameter invalidates existing root")?;
        Ok(new_root)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_convert_param_to_reference(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        param_index: usize,
        param_name: &str,
        region: &str,
        mutable: bool,
    ) -> Result<String> {
        validate_projection_identifier("region parameter", region)?;
        let mut root = self.load_root(input_root)?;
        let old_root = root.clone();
        self.assert_name_points(&root, module, name, symbol)?;
        let callers = self.reverse_dependencies_for_root(&old_root, symbol)?;

        let idx = root_symbol_index(&root, symbol)?;
        let old_signature = root.symbols[idx].signature.clone();
        let old_definition = root.symbols[idx].definition.clone();
        if self.definition_is_external(&old_definition)? {
            bail!("cannot convert parameter on external function {module}.{name}");
        }

        let (mut param_types, return_type) = self.signature_parts(&old_signature)?;
        let effects = self.signature_effects(&old_signature)?;
        let param_name_list = param_names(&root, symbol);
        let actual_param_name = param_name_list
            .get(param_index)
            .ok_or_else(|| anyhow!("{module}.{name} has no parameter index {param_index}"))?;
        if actual_param_name != param_name {
            bail!(
                "parameter index {param_index} is {actual_param_name}, not {param_name} on {module}.{name}"
            );
        }
        let old_param_type = param_types
            .get(param_index)
            .cloned()
            .ok_or_else(|| anyhow!("{module}.{name} has no parameter index {param_index}"))?;
        if matches!(
            self.type_spec_in_root(&root, &old_param_type)?,
            TypeSpec::Reference { .. }
        ) {
            bail!(
                "parameter {module}.{name}.{param_name} is already a reference; convert_by_value_param_to_ref requires a by-value parameter"
            );
        }

        let mut region_params = self.signature_region_params(&old_signature)?;
        self.ensure_signature_region_param(&mut region_params, symbol, region)?;
        let region_scope = region_scope_from_params(&region_params);
        let referent_source =
            self.type_name_in_root_with_regions(&root, module, &old_param_type, &region_scope)?;
        let reference_source = if mutable {
            format!("&'{region} mut {referent_source}")
        } else {
            format!("&'{region} {referent_source}")
        };
        param_types[param_index] = self.resolve_type_in_root_with_regions(
            module,
            &root,
            &reference_source,
            &region_scope,
        )?;
        let new_signature = self.put_signature_with_effects_and_regions(
            &param_types,
            &return_type,
            &effects,
            &region_params,
        )?;
        root.symbols[idx].signature = new_signature;
        upsert_param_names(&mut root, symbol, param_name_list);

        let mut affected = callers.into_iter().collect::<BTreeSet<_>>();
        affected.insert(symbol.to_string());
        for affected_symbol in affected {
            let affected_idx = root_symbol_index(&root, &affected_symbol)?;
            if self.definition_is_external(&root.symbols[affected_idx].definition)? {
                continue;
            }
            let old_entry = old_root
                .symbols
                .iter()
                .find(|entry| entry.symbol == affected_symbol)
                .ok_or_else(|| {
                    anyhow!("affected symbol missing from old root {affected_symbol}")
                })?;
            let affected_module = preferred_module_for_symbol(&old_root, &affected_symbol)?;
            if affected_symbol != symbol {
                self.ensure_function_signature_region(
                    &mut root,
                    &affected_symbol,
                    &affected_module,
                    region,
                )?;
            }

            let current_signature = root.symbols[affected_idx].signature.clone();
            let current_region_params = self.signature_region_params(&current_signature)?;
            let current_region_scope = region_scope_from_params(&current_region_params);
            let current_region_names = current_region_params
                .iter()
                .map(|param| (param.region.clone(), param.name.clone()))
                .collect::<BTreeMap<_, _>>();
            let old_region_names = self
                .signature_region_params(&old_entry.signature)?
                .into_iter()
                .map(|param| (param.region, param.name))
                .collect::<BTreeMap<_, _>>();
            let old_body_hash = self.function_body_hash(&old_entry.definition)?;
            let old_raw_body = self.typed_expr_to_raw_in_module_with_regions(
                &old_body_hash,
                &old_root,
                &affected_module,
                &old_region_names,
            )?;
            let target_name = self.symbol_display_for_module(&root, &affected_module, symbol)?;
            let patched_body = borrow_call_arg_to_calls(
                &old_raw_body,
                &target_name,
                param_index,
                region,
                mutable,
            )?;
            let (current_param_types, current_return_type) =
                self.signature_parts(&current_signature)?;
            let current_param_names = param_names(&root, &affected_symbol);
            let typed_body = self.type_expr_in_module_with_regions_expecting(
                &affected_module,
                &patched_body,
                &root,
                &current_param_names,
                &current_param_types,
                &current_region_scope,
                Some(&current_return_type),
            )?;
            if !self.type_assignable_in_root(&root, &typed_body.type_hash, &current_return_type)? {
                bail!(
                    "converted parameter rewrite changed function {} body type from {} to {}",
                    self.symbol_display(&root, &affected_symbol)?,
                    self.type_name_in_root_with_regions(
                        &root,
                        &affected_module,
                        &current_return_type,
                        &current_region_names,
                    )?,
                    self.type_name_in_root_with_regions(
                        &root,
                        &affected_module,
                        &typed_body.type_hash,
                        &current_region_names,
                    )?
                );
            }
            root.symbols[affected_idx].definition =
                self.put_function_def(&affected_symbol, &current_signature, &typed_body.expr_hash)?;
        }

        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)
            .context("converted parameter invalidates existing root")?;
        Ok(new_root)
    }

    fn ensure_signature_region_param(
        &mut self,
        region_params: &mut Vec<RegionParamDef>,
        owner_symbol: &str,
        region: &str,
    ) -> Result<String> {
        if let Some(existing) = region_params.iter().find(|param| param.name == region) {
            return Ok(existing.region.clone());
        }
        let region_hash = self.put_region_param_birth(
            None,
            owner_symbol,
            &format!("semantic-patch:region:{owner_symbol}:{region}"),
        )?;
        region_params.push(RegionParamDef {
            region: region_hash.clone(),
            name: region.to_string(),
        });
        Ok(region_hash)
    }

    fn ensure_function_signature_region(
        &mut self,
        root: &mut ProgramRootPayload,
        symbol: &str,
        module: &str,
        region: &str,
    ) -> Result<()> {
        let idx = root_symbol_index(root, symbol)?;
        let signature = root.symbols[idx].signature.clone();
        let mut region_params = self.signature_region_params(&signature)?;
        if region_params.iter().any(|param| param.name == region) {
            return Ok(());
        }
        self.ensure_signature_region_param(&mut region_params, symbol, region)?;
        let (param_types, return_type) = self.signature_parts(&signature)?;
        let effects = self.signature_effects(&signature)?;
        let new_signature = self.put_signature_with_effects_and_regions(
            &param_types,
            &return_type,
            &effects,
            &region_params,
        )?;
        root.symbols[idx].signature = new_signature;
        let names = param_names(root, symbol);
        validate_param_names(
            &param_types
                .iter()
                .zip(names.iter())
                .map(|(ty, name)| {
                    Ok(ParamSpec {
                        name: name.clone(),
                        ty: self.type_name_in_root_with_regions(
                            root,
                            module,
                            ty,
                            &region_scope_from_params(&region_params),
                        )?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        )?;
        Ok(())
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
        let live_tests = root
            .tests
            .iter()
            .filter_map(
                |binding| match test_points_to_entry_symbol(self, &binding.test, symbol) {
                    Ok(true) => Some(Ok(binding.name.clone())),
                    Ok(false) => None,
                    Err(err) => Some(Err(err)),
                },
            )
            .collect::<Result<Vec<_>>>()?;
        if !force && (!deps.is_empty() || !live_tests.is_empty()) {
            let mut blockers = deps
                .into_iter()
                .map(|dep| self.symbol_display(&root, &dep))
                .collect::<Result<Vec<_>>>()?;
            blockers.extend(
                live_tests
                    .iter()
                    .map(|test| format!("test:{test}"))
                    .collect::<Vec<_>>(),
            );
            bail!(
                "cannot delete {module}.{name}; live references: {}",
                blockers.join(", ")
            );
        }
        root.symbols.retain(|entry| entry.symbol != symbol);
        root.names.retain(|binding| binding.symbol != symbol);
        root.param_names.retain(|entry| entry.symbol != symbol);
        root.exports.retain(|binding| binding.symbol != symbol);
        if force {
            root.tests
                .retain(|binding| !live_tests.iter().any(|test| test == &binding.name));
        }
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_create_test(
        &mut self,
        input_root: &str,
        name: &str,
        entry_module: &str,
        entry_name: &str,
        entry_symbol: &str,
        category: TestCategory,
        mode: TestMode,
        args: &[TestValue],
        expected: &TestValue,
        native_agreement: bool,
        native_required: bool,
    ) -> Result<String> {
        validate_projection_identifier("test name", name)?;
        let mut root = self.load_root(input_root)?;
        if root.tests.iter().any(|binding| binding.name == name) {
            bail!("test already exists: {name}");
        }
        self.assert_name_points(&root, entry_module, entry_name, entry_symbol)?;
        let entry = self
            .root_symbol(&root, entry_symbol)
            .ok_or_else(|| anyhow!("missing entry symbol {entry_symbol}"))?;
        let (param_types, return_type) = self.signature_parts(&entry.signature)?;
        if param_types.len() != args.len() {
            bail!(
                "test {name} entry {entry_module}.{entry_name} expects {} args, got {}",
                param_types.len(),
                args.len()
            );
        }
        for (idx, (arg, type_hash)) in args.iter().zip(param_types.iter()).enumerate() {
            validate_test_value_for_type(self, &root, arg, type_hash, &format!("argument {idx}"))?;
        }
        validate_test_value_for_type(self, &root, expected, &return_type, "expected value")?;
        let native_agreement = native_agreement || native_required;
        let mode = if native_agreement || native_required {
            TestMode::ReferenceAndNative
        } else {
            mode
        };
        let schema = if mode == TestMode::ReferenceAndNative || native_required {
            crate::model::TEST_CASE_SCHEMA_V2
        } else {
            crate::model::TEST_CASE_SCHEMA_V1
        };
        let case = TestCasePayload {
            schema: schema.to_string(),
            category,
            mode,
            entry_symbol: entry_symbol.to_string(),
            args: args.to_vec(),
            expected: expected.clone(),
            native_agreement,
            native_required,
        };
        self.validate_test_case_for_root(input_root, &root, &case)?;
        let test = self.put_test_case(&case)?;
        root.tests.push(RootTestBinding {
            name: name.to_string(),
            test,
        });
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_delete_test(
        &mut self,
        input_root: &str,
        name: &str,
        test: &str,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        let original_len = root.tests.len();
        root.tests
            .retain(|binding| !(binding.name == name && binding.test == test));
        if root.tests.len() == original_len {
            bail!("test {name} does not point to {test}");
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_merge_branch(
        &mut self,
        input_root: &str,
        source_root_hash: &str,
        merged_root: &ProgramRootPayload,
        object_payloads: &[MergeObjectPayload],
    ) -> Result<String> {
        self.load_root(input_root)?;
        for object in object_payloads {
            let inserted = self.put_object(&object.kind, &object.payload)?;
            if inserted != object.hash {
                bail!(
                    "merge object payload for {} recomputes to {inserted}",
                    object.hash
                );
            }
        }
        for object in object_payloads {
            self.refresh_edges(&object.hash, &object.payload)?;
        }
        for object in object_payloads {
            if object.kind == "ProgramRoot" {
                self.index_root(&object.hash)?;
                self.type_check_root(&object.hash)?;
            }
        }
        self.load_root(source_root_hash)
            .with_context(|| format!("merge source root is not available: {source_root_hash}"))?;

        let new_root = self.put_program_root(merged_root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
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

    pub(crate) fn assert_type_name_points(
        &self,
        root: &ProgramRootPayload,
        module: &str,
        name: &str,
        type_symbol: &str,
    ) -> Result<()> {
        if type_name_points_to_type(root, module, name, type_symbol) {
            Ok(())
        } else {
            bail!("precondition failed: {module}.{name} does not point to {type_symbol}")
        }
    }

    fn create_region_params(
        &mut self,
        parent_history_hash: Option<&str>,
        type_symbol: &str,
        birth_seed: &str,
        region_param_names: &[String],
        region_param_births: Option<&[SymbolBirthSpec]>,
    ) -> Result<(Vec<RegionParamDef>, BTreeMap<String, String>)> {
        validate_region_param_names(region_param_names)?;
        if let Some(region_param_births) = region_param_births
            && region_param_births.len() != region_param_names.len()
        {
            bail!(
                "projection identity region parameter count mismatch: expected {}, got {}",
                region_param_names.len(),
                region_param_births.len()
            );
        }
        let mut params = Vec::with_capacity(region_param_names.len());
        let mut scope = BTreeMap::new();
        for (idx, name) in region_param_names.iter().enumerate() {
            let region = if let Some(region_param_births) = region_param_births {
                self.put_symbol_birth_spec(
                    &region_param_births[idx],
                    "region_param",
                    Some(type_symbol),
                )?
            } else {
                self.put_region_param_birth(
                    parent_history_hash,
                    type_symbol,
                    &format!("{birth_seed}:region:{idx}:{name}"),
                )?
            };
            params.push(RegionParamDef {
                region: region.clone(),
                name: name.clone(),
            });
            scope.insert(name.clone(), region);
        }
        Ok((params, scope))
    }

    #[allow(clippy::too_many_arguments)]
    fn type_definition_from_source(
        &mut self,
        root: &ProgramRootPayload,
        module: &str,
        parent_history_hash: Option<&str>,
        birth_seed: &str,
        type_symbol: &str,
        region_params: Vec<RegionParamDef>,
        region_scope: &BTreeMap<String, String>,
        definition: &TypeDefinitionKind,
        identity: Option<&TypeDefinitionIdentity>,
    ) -> Result<TypeDefinition> {
        match definition {
            TypeDefinitionKind::Record { fields } => {
                if let Some(identity) = identity
                    && identity.member_births.len() != fields.len()
                {
                    bail!(
                        "projection identity record field count mismatch: expected {}, got {}",
                        fields.len(),
                        identity.member_births.len()
                    );
                }
                let fields = fields
                    .iter()
                    .enumerate()
                    .map(|(idx, field)| {
                        validate_projection_identifier("record field", &field.name)?;
                        let member_symbol = if let Some(identity) = identity {
                            self.put_symbol_birth_spec(
                                &identity.member_births[idx],
                                "record_field",
                                Some(type_symbol),
                            )?
                        } else {
                            self.put_record_field_birth(
                                parent_history_hash,
                                type_symbol,
                                &format!("{birth_seed}:field:{idx}:{}", field.name),
                            )?
                        };
                        Ok(TypeMemberDef {
                            member_symbol,
                            name: field.name.clone(),
                            type_hash: self.resolve_type_in_root_with_regions(
                                module,
                                root,
                                &field.ty,
                                region_scope,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(TypeDefinition::Record {
                    type_symbol: type_symbol.to_string(),
                    region_params,
                    fields,
                })
            }
            TypeDefinitionKind::Enum { variants } => {
                if let Some(identity) = identity
                    && identity.member_births.len() != variants.len()
                {
                    bail!(
                        "projection identity enum variant count mismatch: expected {}, got {}",
                        variants.len(),
                        identity.member_births.len()
                    );
                }
                let variants = variants
                    .iter()
                    .enumerate()
                    .map(|(idx, variant)| {
                        validate_projection_identifier("enum variant", &variant.name)?;
                        let member_symbol = if let Some(identity) = identity {
                            self.put_symbol_birth_spec(
                                &identity.member_births[idx],
                                "enum_variant",
                                Some(type_symbol),
                            )?
                        } else {
                            self.put_enum_variant_birth(
                                parent_history_hash,
                                type_symbol,
                                &format!("{birth_seed}:variant:{idx}:{}", variant.name),
                            )?
                        };
                        Ok(TypeMemberDef {
                            member_symbol,
                            name: variant.name.clone(),
                            type_hash: self.resolve_type_in_root_with_regions(
                                module,
                                root,
                                &variant.ty,
                                region_scope,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(TypeDefinition::Enum {
                    type_symbol: type_symbol.to_string(),
                    region_params,
                    variants,
                })
            }
        }
    }

    fn update_type_definition(
        &mut self,
        root: &mut ProgramRootPayload,
        type_symbol: &str,
        definition: TypeDefinition,
    ) -> Result<()> {
        let idx = root_type_index(root, type_symbol)?;
        let type_def = self.put_type_def(type_symbol, &definition)?;
        root.types[idx].type_def = type_def;
        Ok(())
    }

    fn type_definition_for_symbol(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
    ) -> Result<TypeDefinition> {
        let entry = self
            .root_type(root, type_symbol)
            .ok_or_else(|| anyhow!("type missing from root {type_symbol}"))?;
        self.type_definition(&entry.type_def)
    }

    fn rewrite_function_bodies_for_member_rename(
        &mut self,
        old_root: &ProgramRootPayload,
        root: &mut ProgramRootPayload,
        rename: &MemberRename,
    ) -> Result<()> {
        for idx in 0..root.symbols.len() {
            let symbol = root.symbols[idx].symbol.clone();
            let Some(old_entry) = old_root.symbols.iter().find(|entry| entry.symbol == symbol)
            else {
                continue;
            };
            if self.definition_is_external(&old_entry.definition)? {
                continue;
            }
            let signature = root.symbols[idx].signature.clone();
            let (param_types, return_type) = self.signature_parts(&signature)?;
            let region_params = self.signature_region_params(&signature)?;
            let region_scope = region_scope_from_params(&region_params);
            let region_names = region_params
                .iter()
                .map(|param| (param.region.clone(), param.name.clone()))
                .collect::<BTreeMap<_, _>>();
            let module = preferred_module_for_symbol(old_root, &symbol)?;
            let body = self.function_body_hash(&old_entry.definition)?;
            let mut local_names = Vec::new();
            let raw_body = self.rewrite_typed_expr_for_member_rename(
                old_root,
                &module,
                &region_names,
                &mut local_names,
                &body,
                Some(&return_type),
                rename,
            )?;
            let param_name_list = param_names(old_root, &symbol);
            let typed_body = self.type_expr_in_module_with_regions_expecting(
                &module,
                &raw_body,
                root,
                &param_name_list,
                &param_types,
                &region_scope,
                Some(&return_type),
            )?;
            if !self.type_assignable_in_root(root, &typed_body.type_hash, &return_type)? {
                bail!(
                    "renamed member rewrite changed function {} body type from {} to {}",
                    self.symbol_display(old_root, &symbol)?,
                    self.type_name(&return_type)?,
                    self.type_name(&typed_body.type_hash)?
                );
            }
            root.symbols[idx].definition =
                self.put_function_def(&symbol, &signature, &typed_body.expr_hash)?;
        }
        Ok(())
    }

    #[allow(clippy::replace_box, clippy::too_many_arguments)]
    fn rewrite_typed_expr_for_member_rename(
        &self,
        old_root: &ProgramRootPayload,
        module: &str,
        region_names: &BTreeMap<String, String>,
        local_names: &mut Vec<String>,
        expr_hash: &str,
        expected_type: Option<&str>,
        rename: &MemberRename,
    ) -> Result<RawExpr> {
        let payload = self.get_payload(expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        let mut raw = self.typed_expr_to_raw_in_module_with_regions_and_locals(
            expr_hash,
            old_root,
            module,
            region_names,
            local_names,
        )?;
        match (&mut raw, expr_kind) {
            (RawExpr::Call { args, .. }, "call") => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let callee = old_root
                    .symbols
                    .iter()
                    .find(|entry| entry.symbol == symbol)
                    .ok_or_else(|| anyhow!("call target missing from old root {symbol}"))?;
                let (expected_params, _) = self.signature_parts(&callee.signature)?;
                let arg_hashes = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                *args = arg_hashes
                    .iter()
                    .enumerate()
                    .map(|(idx, value)| {
                        let arg = value
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        self.rewrite_typed_expr_for_member_rename(
                            old_root,
                            module,
                            region_names,
                            local_names,
                            arg,
                            expected_params.get(idx).map(String::as_str),
                            rename,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            (RawExpr::Binary { left, right, .. }, "binary") => {
                *left = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "left",
                    None,
                    rename,
                )?);
                *right = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "right",
                    None,
                    rename,
                )?);
            }
            (RawExpr::Unary { expr, .. }, "unary") => {
                *expr = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "expr",
                    None,
                    rename,
                )?);
            }
            (RawExpr::BorrowShared { target, .. }, "borrow_shared")
            | (RawExpr::BorrowMut { target, .. }, "borrow_mut") => {
                *target = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "target",
                    None,
                    rename,
                )?);
            }
            (RawExpr::Assign { target, value }, "assign") => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("assign missing target"))?;
                let target_type = self.expr_declared_type(target_hash)?;
                *target = Box::new(self.rewrite_typed_expr_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    target_hash,
                    None,
                    rename,
                )?);
                *value = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "value",
                    Some(&target_type),
                    rename,
                )?);
            }
            (
                RawExpr::Let {
                    name, value, body, ..
                },
                "let",
            ) => {
                let binding_type = payload
                    .get("binding_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_type"))?;
                *value = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "value",
                    Some(binding_type),
                    rename,
                )?);
                local_names.push(name.clone());
                let rewritten_body = self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "body",
                    expected_type,
                    rename,
                );
                local_names.pop();
                *body = Box::new(rewritten_body?);
            }
            (
                RawExpr::If {
                    cond,
                    then_expr,
                    else_expr,
                },
                "if",
            ) => {
                *cond = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "cond",
                    None,
                    rename,
                )?);
                *then_expr = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "then",
                    expected_type,
                    rename,
                )?);
                *else_expr = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "else",
                    expected_type,
                    rename,
                )?);
            }
            (
                RawExpr::Fold {
                    item,
                    target,
                    acc,
                    init,
                    body,
                },
                "fold",
            ) => {
                *target = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "target",
                    None,
                    rename,
                )?);
                let acc_type = payload
                    .get("acc_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing acc_type"))?;
                *init = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "init",
                    Some(acc_type),
                    rename,
                )?);
                local_names.push(item.clone());
                local_names.push(acc.clone());
                let rewritten_body = self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "body",
                    Some(acc_type),
                    rename,
                );
                local_names.pop();
                local_names.pop();
                *body = Box::new(rewritten_body?);
            }
            (RawExpr::Array { elements }, "array_literal") => {
                let element_payloads = payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?;
                if element_payloads.len() != elements.len() {
                    return Err(anyhow!(
                        "array literal raw/typed element count mismatch: {} != {}",
                        elements.len(),
                        element_payloads.len()
                    ));
                }
                let element_type = payload
                    .get("element_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_literal missing element_type"))?;
                *elements = element_payloads
                    .iter()
                    .map(|element| {
                        let value_hash = element
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array element missing value"))?;
                        let expected = element
                            .get("type")
                            .and_then(JsonValue::as_str)
                            .unwrap_or(element_type);
                        self.rewrite_typed_expr_for_member_rename(
                            old_root,
                            module,
                            region_names,
                            local_names,
                            value_hash,
                            Some(expected),
                            rename,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            (RawExpr::Index { target, index }, "array_index") => {
                *target = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "target",
                    None,
                    rename,
                )?);
                *index = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "index",
                    None,
                    rename,
                )?);
            }
            (RawExpr::Record { fields }, "record_literal") => {
                let expected_fields =
                    expected_type.and_then(|ty| self.record_fields_by_name(old_root, ty).ok());
                let field_payloads = payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?;
                *fields = field_payloads
                    .iter()
                    .map(|field| {
                        let old_name = field
                            .get("name")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing name"))?;
                        let value_hash = field
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing value"))?;
                        let expected_field = expected_fields
                            .as_ref()
                            .and_then(|fields| fields.get(old_name));
                        let name = if self.record_field_matches_rename(
                            old_root,
                            expected_type,
                            old_name,
                            rename,
                        )? {
                            rename.new_name().to_string()
                        } else {
                            old_name.to_string()
                        };
                        Ok(RawRecordField {
                            name,
                            value: self.rewrite_typed_expr_for_member_rename(
                                old_root,
                                module,
                                region_names,
                                local_names,
                                value_hash,
                                expected_field.map(String::as_str),
                                rename,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            (RawExpr::FieldAccess { target, field }, "field_access") => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                let target_type = self.expr_declared_type(target_hash)?;
                *target = Box::new(self.rewrite_typed_expr_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    target_hash,
                    None,
                    rename,
                )?);
                if self.record_field_matches_rename(old_root, Some(&target_type), field, rename)? {
                    *field = rename.new_name().to_string();
                }
            }
            (RawExpr::EnumConstruct { variant, value, .. }, "enum_construct") => {
                let enum_type = payload
                    .get("enum_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing enum_type"))?;
                let old_variant = variant.clone();
                let variant_type =
                    self.enum_variant_type_in_root(old_root, enum_type, &old_variant)?;
                if self.enum_variant_matches_rename(old_root, enum_type, &old_variant, rename)? {
                    *variant = rename.new_name().to_string();
                }
                *value = Box::new(self.rewrite_expr_child_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    &payload,
                    "value",
                    Some(&variant_type),
                    rename,
                )?);
            }
            (RawExpr::Case { expr, arms }, "case") => {
                let scrutinee_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                let scrutinee_type = self.expr_declared_type(scrutinee_hash)?;
                *expr = Box::new(self.rewrite_typed_expr_for_member_rename(
                    old_root,
                    module,
                    region_names,
                    local_names,
                    scrutinee_hash,
                    None,
                    rename,
                )?);
                let arm_payloads = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?;
                let mut rewritten_arms = Vec::new();
                for arm in arm_payloads {
                    let body_hash = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    let is_default = arm.get("default").and_then(JsonValue::as_bool) == Some(true);
                    let variant = if is_default {
                        None
                    } else {
                        let old_variant = arm
                            .get("variant")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing variant"))?;
                        Some(
                            if self.enum_variant_matches_rename(
                                old_root,
                                &scrutinee_type,
                                old_variant,
                                rename,
                            )? {
                                rename.new_name().to_string()
                            } else {
                                old_variant.to_string()
                            },
                        )
                    };
                    let binding = arm
                        .get("binding_name")
                        .and_then(JsonValue::as_str)
                        .map(str::to_string);
                    if let Some(binding) = &binding {
                        local_names.push(binding.clone());
                    }
                    let body = self.rewrite_typed_expr_for_member_rename(
                        old_root,
                        module,
                        region_names,
                        local_names,
                        body_hash,
                        expected_type,
                        rename,
                    );
                    if binding.is_some() {
                        local_names.pop();
                    }
                    rewritten_arms.push(RawCaseArm {
                        variant,
                        default: is_default,
                        binding,
                        body: body?,
                    });
                }
                *arms = rewritten_arms;
            }
            _ => {}
        }
        Ok(raw)
    }

    #[allow(clippy::too_many_arguments)]
    fn rewrite_expr_child_for_member_rename(
        &self,
        old_root: &ProgramRootPayload,
        module: &str,
        region_names: &BTreeMap<String, String>,
        local_names: &mut Vec<String>,
        payload: &JsonValue,
        key: &str,
        expected_type: Option<&str>,
        rename: &MemberRename,
    ) -> Result<RawExpr> {
        let child = payload
            .get(key)
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing {key}"))?;
        self.rewrite_typed_expr_for_member_rename(
            old_root,
            module,
            region_names,
            local_names,
            child,
            expected_type,
            rename,
        )
    }

    fn record_fields_by_name(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<BTreeMap<String, String>> {
        let TypeSpec::Record(fields) = self.type_spec_in_root(root, type_hash)? else {
            return Ok(BTreeMap::new());
        };
        Ok(fields
            .into_iter()
            .map(|field| (field.name, field.type_hash))
            .collect())
    }

    fn record_field_matches_rename(
        &self,
        root: &ProgramRootPayload,
        type_hash: Option<&str>,
        field: &str,
        rename: &MemberRename,
    ) -> Result<bool> {
        let MemberRename::Field {
            type_symbol,
            member_symbol,
            old_name,
            ..
        } = rename
        else {
            return Ok(false);
        };
        if field != old_name {
            return Ok(false);
        }
        let Some(type_hash) = type_hash else {
            return Ok(false);
        };
        Ok(self.record_field_member_for_type(root, type_hash, field)?
            == Some((type_symbol.clone(), member_symbol.clone())))
    }

    fn record_field_member_for_type(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        field: &str,
    ) -> Result<Option<(String, String)>> {
        match self.type_spec(type_hash)? {
            TypeSpec::Reference { referent, .. } => {
                self.record_field_member_for_type(root, &referent, field)
            }
            TypeSpec::Named { type_symbol, .. } => {
                let TypeDefinition::Record { fields, .. } =
                    self.type_definition_for_symbol(root, &type_symbol)?
                else {
                    return Ok(None);
                };
                Ok(fields
                    .into_iter()
                    .find(|candidate| candidate.name == field)
                    .map(|field| (type_symbol, field.member_symbol)))
            }
            _ => Ok(None),
        }
    }

    fn enum_variant_matches_rename(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        variant: &str,
        rename: &MemberRename,
    ) -> Result<bool> {
        let MemberRename::Variant {
            type_symbol,
            member_symbol,
            old_name,
            ..
        } = rename
        else {
            return Ok(false);
        };
        if variant != old_name {
            return Ok(false);
        }
        Ok(self.enum_variant_member_for_type(root, type_hash, variant)?
            == Some((type_symbol.clone(), member_symbol.clone())))
    }

    fn enum_variant_member_for_type(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        variant: &str,
    ) -> Result<Option<(String, String)>> {
        match self.type_spec(type_hash)? {
            TypeSpec::Named { type_symbol, .. } => {
                let TypeDefinition::Enum { variants, .. } =
                    self.type_definition_for_symbol(root, &type_symbol)?
                else {
                    return Ok(None);
                };
                Ok(variants
                    .into_iter()
                    .find(|candidate| candidate.name == variant)
                    .map(|variant| (type_symbol, variant.member_symbol)))
            }
            _ => Ok(None),
        }
    }

    fn type_source_matches(
        &self,
        root: &ProgramRootPayload,
        module: &str,
        name: &str,
        region_param_names: &[String],
        expected: &TypeDefinitionKind,
    ) -> Result<bool> {
        let Some(type_symbol) = type_symbol_for_name(root, module, name) else {
            return Ok(false);
        };
        let definition = self.type_definition_for_symbol(root, &type_symbol)?;
        let actual_region_names = definition
            .region_params()
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        if actual_region_names != region_param_names {
            return Ok(false);
        }
        let region_scope = region_scope_from_params(definition.region_params());
        match (definition, expected) {
            (
                TypeDefinition::Record { fields, .. },
                TypeDefinitionKind::Record { fields: expected },
            ) => self.member_specs_match(root, module, &region_scope, &fields, expected),
            (
                TypeDefinition::Enum { variants, .. },
                TypeDefinitionKind::Enum { variants: expected },
            ) => self.member_specs_match(root, module, &region_scope, &variants, expected),
            _ => Ok(false),
        }
    }

    fn member_specs_match(
        &self,
        root: &ProgramRootPayload,
        module: &str,
        region_scope: &BTreeMap<String, String>,
        actual: &[TypeMemberDef],
        expected: &[TypeMemberSpec],
    ) -> Result<bool> {
        if actual.len() != expected.len() {
            return Ok(false);
        }
        for (actual, expected) in actual.iter().zip(expected.iter()) {
            if actual.name != expected.name {
                return Ok(false);
            }
            let expected_hash = self.type_hash_for_source_in_root_with_regions(
                module,
                root,
                &expected.ty,
                region_scope,
            )?;
            if actual.type_hash != expected_hash {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn field_points_to_symbol(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        name: &str,
        field_symbol: &str,
    ) -> Result<bool> {
        let definition = self.type_definition_for_symbol(root, type_symbol)?;
        let TypeDefinition::Record { fields, .. } = definition else {
            return Ok(false);
        };
        Ok(fields
            .iter()
            .any(|field| field.name == name && field.member_symbol == field_symbol))
    }

    fn field_name_is_available(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        name: &str,
    ) -> Result<bool> {
        let definition = self.type_definition_for_symbol(root, type_symbol)?;
        let TypeDefinition::Record { fields, .. } = definition else {
            return Ok(false);
        };
        Ok(!fields.iter().any(|field| field.name == name))
    }

    pub(crate) fn field_symbol_by_name(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        name: &str,
    ) -> Result<String> {
        let definition = self.type_definition_for_symbol(root, type_symbol)?;
        let TypeDefinition::Record { fields, .. } = definition else {
            bail!("type is not a record {type_symbol}");
        };
        fields
            .into_iter()
            .find(|field| field.name == name)
            .map(|field| field.member_symbol)
            .ok_or_else(|| anyhow!("record has no field {name}"))
    }

    fn variant_points_to_symbol(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        name: &str,
        variant_symbol: &str,
    ) -> Result<bool> {
        let definition = self.type_definition_for_symbol(root, type_symbol)?;
        let TypeDefinition::Enum { variants, .. } = definition else {
            return Ok(false);
        };
        Ok(variants
            .iter()
            .any(|variant| variant.name == name && variant.member_symbol == variant_symbol))
    }

    fn variant_name_is_available(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        name: &str,
    ) -> Result<bool> {
        let definition = self.type_definition_for_symbol(root, type_symbol)?;
        let TypeDefinition::Enum { variants, .. } = definition else {
            return Ok(false);
        };
        Ok(!variants.iter().any(|variant| variant.name == name))
    }

    pub(crate) fn variant_symbol_by_name(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        name: &str,
    ) -> Result<String> {
        let definition = self.type_definition_for_symbol(root, type_symbol)?;
        let TypeDefinition::Enum { variants, .. } = definition else {
            bail!("type is not an enum {type_symbol}");
        };
        variants
            .into_iter()
            .find(|variant| variant.name == name)
            .map(|variant| variant.member_symbol)
            .ok_or_else(|| anyhow!("enum has no variant {name}"))
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
        self.history_branch_json(MAIN_BRANCH)
    }

    pub(crate) fn history_branch_json(&self, branch_name: &str) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let chain = self.history_chain(branch_name)?;
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
                "branch": branch_name,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
                "migrations": migrations,
            }))
        ))
    }

    pub fn branches(&self) -> Result<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, root_hash, history_hash FROM branches ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = String::new();
        for row in rows {
            let (name, root_hash, history_hash) = row?;
            out.push_str(&format!(
                "{name} root {root_hash} history {}\n",
                history_hash.unwrap_or_else(|| "none".to_string())
            ));
        }
        if out.is_empty() {
            out.push_str("branches empty\n");
        }
        Ok(out)
    }

    pub fn branches_json(&self) -> Result<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, root_hash, history_hash FROM branches ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "root_hash": row.get::<_, String>(1)?,
                "history_hash": row.get::<_, Option<String>>(2)?,
            }))
        })?;
        let branches = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(format!(
            "{}\n",
            canonical_json(&json!({
                "schema": "codedb/branches/v1",
                "branches": branches,
            }))
        ))
    }

    pub fn export_history_branch(&self, branch: &str) -> Result<String> {
        let state = self.branch(branch)?;
        let chain = self.history_chain(branch)?;
        let mut out = String::new();
        out.push_str(&canonical_json(&json!({
            "schema": HISTORY_EXPORT_SCHEMA,
            "branch": branch,
            "root_hash": state.root_hash,
            "history_hash": state.history_hash,
            "migration_count": chain.len(),
        })));
        out.push('\n');
        for (sequence, item) in chain.iter().enumerate() {
            let row = self.history_export_migration_line(sequence, item)?;
            out.push_str(&canonical_json(&row));
            out.push('\n');
        }
        Ok(out)
    }

    pub fn import_history_file(&mut self, path: &Path) -> Result<String> {
        self.ensure_initialized()?;
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        self.import_history_str(&text)
            .with_context(|| format!("failed to import {}", path.display()))
    }

    pub fn import_history_str(&mut self, text: &str) -> Result<String> {
        self.ensure_initialized()?;
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.import_history_str_in_tx(text);
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

    fn history_export_migration_line(
        &self,
        sequence: usize,
        item: &HistoryItem,
    ) -> Result<JsonValue> {
        let (parent_history_hash, preconditions_json, postconditions_json, agent_json): (
            Option<String>,
            String,
            String,
            String,
        ) = self.conn.query_row(
            "SELECT parent_history_hash, preconditions_json, postconditions_json, agent_json
             FROM migrations WHERE hash = ?1",
            params![&item.migration_hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok(json!({
            "schema": HISTORY_EXPORT_MIGRATION_SCHEMA,
            "sequence": sequence,
            "migration_hash": item.migration_hash,
            "history_hash": item.history_hash,
            "parent_history_hash": parent_history_hash,
            "input_root_hash": item.input_root,
            "output_root_hash": item.output_root,
            "operation_kind": item.operation_kind,
            "operation": serde_json::to_value(&item.operation)?,
            "preconditions": serde_json::from_str::<JsonValue>(&preconditions_json)?,
            "postconditions": serde_json::from_str::<JsonValue>(&postconditions_json)?,
            "agent": serde_json::from_str::<JsonValue>(&agent_json)?,
        }))
    }

    fn import_history_str_in_tx(&mut self, text: &str) -> Result<String> {
        let mut lines = text.lines();
        let header_line = lines
            .next()
            .ok_or_else(|| anyhow!("history import is empty"))?;
        let header = parse_canonical_ndjson_line(header_line, "history export header")?;
        reject_unknown_fields(
            &header,
            "history export header",
            &[
                "schema",
                "branch",
                "root_hash",
                "history_hash",
                "migration_count",
            ],
        )?;
        if header.get("schema").and_then(JsonValue::as_str) != Some(HISTORY_EXPORT_SCHEMA) {
            bail!("unsupported history export schema");
        }
        let branch = header
            .get("branch")
            .and_then(JsonValue::as_str)
            .unwrap_or(MAIN_BRANCH);
        if branch != MAIN_BRANCH {
            bail!("only branch {MAIN_BRANCH:?} is supported by import-history, got {branch:?}");
        }
        let expected_root = required_string(&header, "root_hash")?;
        let expected_history = optional_string(&header, "history_hash")?;
        let expected_count = header
            .get("migration_count")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("history export header missing migration_count"))?
            as usize;

        let initial_branch = self.branch(MAIN_BRANCH)?;
        if initial_branch.history_hash.is_some() {
            bail!("import-history requires an empty branch history");
        }

        let mut current_root = initial_branch.root_hash;
        let mut current_history: Option<String> = None;
        let mut imported_count = 0usize;

        for (sequence, line) in lines.enumerate() {
            let row = parse_canonical_ndjson_line(line, "history export migration")?;
            self.import_history_migration_row(
                sequence,
                &row,
                &mut current_root,
                &mut current_history,
            )?;
            imported_count += 1;
        }

        if imported_count != expected_count {
            bail!(
                "bad_history_link: expected {expected_count} migrations, imported {imported_count}"
            );
        }
        if current_root != expected_root {
            bail!(
                "bad_history_link: final root mismatch, expected {expected_root}, imported {current_root}"
            );
        }
        if current_history != expected_history {
            bail!(
                "bad_history_link: final history mismatch, expected {:?}, imported {:?}",
                expected_history,
                current_history
            );
        }

        if let Some(history_hash) = &current_history {
            self.update_branch(MAIN_BRANCH, &current_root, history_hash)?;
        }
        Ok(format!(
            "imported history\nroot {}\nhistory {}\nmigrations {}\n",
            current_root,
            current_history.unwrap_or_else(|| "none".to_string()),
            imported_count
        ))
    }

    fn import_history_migration_row(
        &mut self,
        expected_sequence: usize,
        row: &JsonValue,
        current_root: &mut String,
        current_history: &mut Option<String>,
    ) -> Result<()> {
        reject_unknown_fields(
            row,
            "history export migration",
            &[
                "schema",
                "sequence",
                "migration_hash",
                "history_hash",
                "parent_history_hash",
                "input_root_hash",
                "output_root_hash",
                "operation_kind",
                "operation",
                "preconditions",
                "postconditions",
                "agent",
            ],
        )?;
        if row.get("schema").and_then(JsonValue::as_str) != Some(HISTORY_EXPORT_MIGRATION_SCHEMA) {
            bail!("unsupported history migration export schema");
        }
        let sequence =
            row.get("sequence")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| anyhow!("history migration missing sequence"))? as usize;
        if sequence != expected_sequence {
            bail!("bad_history_link: expected sequence {expected_sequence}, got {sequence}");
        }

        let migration_hash_value = required_string(row, "migration_hash")?;
        let history_hash_value = required_string(row, "history_hash")?;
        let parent_history_hash = optional_string(row, "parent_history_hash")?;
        if parent_history_hash != *current_history {
            bail!(
                "bad_history_link: migration {migration_hash_value} parent {:?} does not match current {:?}",
                parent_history_hash,
                current_history
            );
        }
        let input_root = required_string(row, "input_root_hash")?;
        if input_root != *current_root {
            bail!(
                "bad_history_link: migration {migration_hash_value} expected input {input_root}, import has {current_root}"
            );
        }
        let output_root = required_string(row, "output_root_hash")?;
        let operation_kind = required_string(row, "operation_kind")?;
        let operation_value = row
            .get("operation")
            .cloned()
            .ok_or_else(|| anyhow!("history migration missing operation"))?;
        let operation: Operation = serde_json::from_value(operation_value.clone())?;
        if operation.kind_name() != operation_kind {
            bail!(
                "bad_history_link: operation kind mismatch for {migration_hash_value}: row has {operation_kind}, operation has {}",
                operation.kind_name()
            );
        }
        let preconditions = row
            .get("preconditions")
            .cloned()
            .ok_or_else(|| anyhow!("history migration missing preconditions"))?;
        let postconditions = row
            .get("postconditions")
            .cloned()
            .ok_or_else(|| anyhow!("history migration missing postconditions"))?;
        let agent = row.get("agent").cloned().unwrap_or_else(|| json!({}));

        let recomputed_migration = migration_hash(
            current_history.as_deref(),
            current_root,
            &output_root,
            &operation_value,
            &preconditions,
            &postconditions,
        );
        if recomputed_migration != migration_hash_value {
            bail!(
                "bad_history_link: migration {migration_hash_value} recomputes to {recomputed_migration}"
            );
        }

        let recomputed_preconditions = canonical_json(&serde_json::to_value(
            self.preconditions_for(current_root, &operation),
        )?);
        if recomputed_preconditions != canonical_json(&preconditions) {
            bail!("bad_history_link: preconditions changed for {migration_hash_value}");
        }
        let failed_preconditions = self.failed_preconditions(
            current_root,
            &self.preconditions_for(current_root, &operation),
        )?;
        if !failed_preconditions.is_empty() {
            bail!(
                "semantic_conflict: migration {migration_hash_value} failed preconditions {}",
                condition_names(&failed_preconditions)
            );
        }

        let produced =
            self.apply_operation_to_root(current_root, current_history.as_deref(), &operation)?;
        if produced != output_root {
            bail!(
                "bad_history_link: migration {migration_hash_value} expected output {output_root}, produced {produced}"
            );
        }
        let recomputed_postconditions = canonical_json(&serde_json::to_value(
            self.postconditions_for(&produced, &operation),
        )?);
        if recomputed_postconditions != canonical_json(&postconditions) {
            bail!("bad_history_link: postconditions changed for {migration_hash_value}");
        }
        let failed_postconditions =
            self.failed_postconditions(&produced, &self.postconditions_for(&produced, &operation))?;
        if !failed_postconditions.is_empty() {
            bail!(
                "semantic_conflict: migration {migration_hash_value} failed postconditions {}",
                condition_names(&failed_postconditions)
            );
        }

        let recomputed_history =
            history_hash(current_history.as_deref(), &migration_hash_value, &produced);
        if recomputed_history != history_hash_value {
            bail!(
                "bad_history_link: history {history_hash_value} recomputes to {recomputed_history}"
            );
        }

        self.conn.execute(
            "INSERT OR IGNORE INTO migrations
             (hash, parent_history_hash, input_root_hash, output_root_hash,
              operation_kind, operation_json, preconditions_json, postconditions_json, agent_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &migration_hash_value,
                current_history.as_deref(),
                current_root.as_str(),
                &produced,
                &operation_kind,
                canonical_json(&operation_value),
                canonical_json(&preconditions),
                canonical_json(&postconditions),
                canonical_json(&agent),
            ],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO histories
             (history_hash, parent_history_hash, migration_hash, output_root_hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                &history_hash_value,
                current_history.as_deref(),
                &migration_hash_value,
                &produced
            ],
        )?;

        *current_root = produced;
        *current_history = Some(history_hash_value);
        Ok(())
    }

    pub fn replay_main_branch(&mut self) -> Result<String> {
        self.ensure_initialized()?;
        self.replay_main_branch_without_init()
    }

    pub(crate) fn replay_main_branch_without_init(&mut self) -> Result<String> {
        let expected = self.branch(MAIN_BRANCH)?;
        let chain = self.history_chain(MAIN_BRANCH)?;
        let mut current_root = self.put_program_root(&ProgramRootPayload {
            symbols: vec![],
            types: vec![],
            names: vec![],
            type_names: vec![],
            param_names: vec![],
            exports: vec![],
            tests: vec![],
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

    pub(crate) fn history_chain(&self, branch: &str) -> Result<Vec<HistoryItem>> {
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
pub(crate) struct HistoryItem {
    pub(crate) history_hash: String,
    pub(crate) migration_hash: String,
    pub(crate) input_root: String,
    pub(crate) output_root: String,
    pub(crate) operation_kind: String,
    pub(crate) operation: Operation,
}

fn parse_canonical_ndjson_line(line: &str, label: &str) -> Result<JsonValue> {
    let value: JsonValue =
        serde_json::from_str(line).with_context(|| format!("invalid {label}"))?;
    let canonical = canonical_json(&value);
    if canonical != line {
        bail!("{label} is not canonical");
    }
    Ok(value)
}

fn reject_unknown_fields(value: &JsonValue, label: &str, allowed: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{label} must be a JSON object"))?;
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    for key in object.keys() {
        if !allowed.contains(key.as_str()) {
            bail!("{label} has unknown field {key:?}");
        }
    }
    Ok(())
}

fn required_string(value: &JsonValue, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing string field {field}"))
}

fn optional_string(value: &JsonValue, field: &str) -> Result<Option<String>> {
    match value.get(field) {
        Some(JsonValue::String(text)) => Ok(Some(text.clone())),
        Some(JsonValue::Null) | None => Ok(None),
        Some(_) => bail!("field {field} must be string or null"),
    }
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

fn type_name_points_to_type(
    root: &ProgramRootPayload,
    module: &str,
    name: &str,
    type_symbol: &str,
) -> bool {
    root.type_names.iter().any(|binding| {
        binding.module == module
            && binding.display_name == name
            && binding.type_symbol == type_symbol
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

fn preferred_type_name_points_to_type(
    root: &ProgramRootPayload,
    module: &str,
    name: &str,
    type_symbol: &str,
) -> bool {
    root.type_names.iter().any(|binding| {
        binding.module == module
            && binding.display_name == name
            && binding.type_symbol == type_symbol
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

fn type_symbol_for_name(root: &ProgramRootPayload, module: &str, name: &str) -> Option<String> {
    root.type_names
        .iter()
        .find(|binding| binding.module == module && binding.display_name == name)
        .map(|binding| binding.type_symbol.clone())
}

fn preferred_module_for_symbol(root: &ProgramRootPayload, symbol: &str) -> Result<String> {
    root.names
        .iter()
        .find(|binding| binding.symbol == symbol && binding.is_preferred)
        .or_else(|| root.names.iter().find(|binding| binding.symbol == symbol))
        .map(|binding| binding.module.clone())
        .ok_or_else(|| anyhow!("symbol {symbol} has no name binding"))
}

fn export_points_to_symbol(root: &ProgramRootPayload, name: &str, symbol: &str) -> bool {
    root.exports
        .iter()
        .any(|binding| binding.exported_name == name && binding.symbol == symbol)
}

fn test_name_points_to_test(root: &ProgramRootPayload, name: &str, test: &str) -> bool {
    root.tests
        .iter()
        .any(|binding| binding.name == name && binding.test == test)
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

fn validate_region_param_names(params: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in params {
        validate_projection_identifier("region parameter", name)?;
        if !seen.insert(name.clone()) {
            bail!("duplicate region parameter {name}");
        }
    }
    Ok(())
}

fn validate_type_member_specs(definition: &TypeDefinitionKind) -> Result<()> {
    match definition {
        TypeDefinitionKind::Record { fields } => {
            if fields.is_empty() {
                bail!("record fields must not be empty");
            }
            validate_member_specs("record field", fields)
        }
        TypeDefinitionKind::Enum { variants } => {
            if variants.is_empty() {
                bail!("enum variants must not be empty");
            }
            validate_member_specs("enum variant", variants)
        }
    }
}

fn validate_member_specs(label: &str, members: &[TypeMemberSpec]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for member in members {
        validate_projection_identifier(label, &member.name)?;
        if !seen.insert(member.name.clone()) {
            bail!("duplicate {label} {}", member.name);
        }
    }
    Ok(())
}

fn region_scope_from_params(params: &[RegionParamDef]) -> BTreeMap<String, String> {
    params
        .iter()
        .map(|param| (param.name.clone(), param.region.clone()))
        .collect()
}

impl CodeDb {
    fn function_region_params(
        &mut self,
        parent_history_hash: Option<&str>,
        owner_symbol: &str,
        birth_seed: &str,
        names: &[String],
    ) -> Result<Vec<RegionParamDef>> {
        names
            .iter()
            .enumerate()
            .map(|(idx, name)| {
                Ok(RegionParamDef {
                    region: self.put_region_param_birth(
                        parent_history_hash,
                        owner_symbol,
                        &format!("{birth_seed}:region:{idx}:{name}"),
                    )?,
                    name: name.clone(),
                })
            })
            .collect()
    }
}

fn borrow_call_arg_to_calls(
    expr: &RawExpr,
    target_name: &str,
    param_index: usize,
    region: &str,
    mutable: bool,
) -> Result<RawExpr> {
    Ok(match expr {
        RawExpr::LiteralI64 { value } => RawExpr::LiteralI64 {
            value: value.clone(),
        },
        RawExpr::LiteralBool { value } => RawExpr::LiteralBool { value: *value },
        RawExpr::LiteralString { value } => RawExpr::LiteralString {
            value: value.clone(),
        },
        RawExpr::LiteralBytes { bytes_hex } => RawExpr::LiteralBytes {
            bytes_hex: bytes_hex.clone(),
        },
        RawExpr::Unit => RawExpr::Unit,
        RawExpr::ParamRef { index } => RawExpr::ParamRef { index: *index },
        RawExpr::ParamName { name } => RawExpr::ParamName { name: name.clone() },
        RawExpr::Call { name, args } => {
            let mut args = args
                .iter()
                .map(|arg| borrow_call_arg_to_calls(arg, target_name, param_index, region, mutable))
                .collect::<Result<Vec<_>>>()?;
            if name == target_name {
                let arg = args.get_mut(param_index).ok_or_else(|| {
                    anyhow!("call to {target_name} has no argument index {param_index}")
                })?;
                let target = Box::new(arg.clone());
                *arg = if mutable {
                    RawExpr::BorrowMut {
                        region: Some(region.to_string()),
                        target,
                    }
                } else {
                    RawExpr::BorrowShared {
                        region: Some(region.to_string()),
                        target,
                    }
                };
            }
            RawExpr::Call {
                name: name.clone(),
                args,
            }
        }
        RawExpr::Binary { op, left, right } => RawExpr::Binary {
            op: op.clone(),
            left: Box::new(borrow_call_arg_to_calls(
                left,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            right: Box::new(borrow_call_arg_to_calls(
                right,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::Unary { op, expr } => RawExpr::Unary {
            op: op.clone(),
            expr: Box::new(borrow_call_arg_to_calls(
                expr,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::BorrowShared { region: r, target } => RawExpr::BorrowShared {
            region: r.clone(),
            target: Box::new(borrow_call_arg_to_calls(
                target,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::BorrowMut { region: r, target } => RawExpr::BorrowMut {
            region: r.clone(),
            target: Box::new(borrow_call_arg_to_calls(
                target,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::Assign { target, value } => RawExpr::Assign {
            target: Box::new(borrow_call_arg_to_calls(
                target,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            value: Box::new(borrow_call_arg_to_calls(
                value,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::Let {
            name,
            ty,
            value,
            body,
        } => RawExpr::Let {
            name: name.clone(),
            ty: ty.clone(),
            value: Box::new(borrow_call_arg_to_calls(
                value,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            body: Box::new(borrow_call_arg_to_calls(
                body,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::If {
            cond,
            then_expr,
            else_expr,
        } => RawExpr::If {
            cond: Box::new(borrow_call_arg_to_calls(
                cond,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            then_expr: Box::new(borrow_call_arg_to_calls(
                then_expr,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            else_expr: Box::new(borrow_call_arg_to_calls(
                else_expr,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::Fold {
            item,
            target,
            acc,
            init,
            body,
        } => RawExpr::Fold {
            item: item.clone(),
            target: Box::new(borrow_call_arg_to_calls(
                target,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            acc: acc.clone(),
            init: Box::new(borrow_call_arg_to_calls(
                init,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            body: Box::new(borrow_call_arg_to_calls(
                body,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::Array { elements } => RawExpr::Array {
            elements: elements
                .iter()
                .map(|element| {
                    borrow_call_arg_to_calls(element, target_name, param_index, region, mutable)
                })
                .collect::<Result<Vec<_>>>()?,
        },
        RawExpr::Index { target, index } => RawExpr::Index {
            target: Box::new(borrow_call_arg_to_calls(
                target,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            index: Box::new(borrow_call_arg_to_calls(
                index,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::Record { fields } => RawExpr::Record {
            fields: fields
                .iter()
                .map(|field| {
                    Ok(RawRecordField {
                        name: field.name.clone(),
                        value: borrow_call_arg_to_calls(
                            &field.value,
                            target_name,
                            param_index,
                            region,
                            mutable,
                        )?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        },
        RawExpr::FieldAccess { target, field } => RawExpr::FieldAccess {
            target: Box::new(borrow_call_arg_to_calls(
                target,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            field: field.clone(),
        },
        RawExpr::EnumConstruct {
            enum_type,
            variant,
            value,
        } => RawExpr::EnumConstruct {
            enum_type: enum_type.clone(),
            variant: variant.clone(),
            value: Box::new(borrow_call_arg_to_calls(
                value,
                target_name,
                param_index,
                region,
                mutable,
            )?),
        },
        RawExpr::Case { expr, arms } => RawExpr::Case {
            expr: Box::new(borrow_call_arg_to_calls(
                expr,
                target_name,
                param_index,
                region,
                mutable,
            )?),
            arms: arms
                .iter()
                .map(|arm| {
                    Ok(RawCaseArm {
                        variant: arm.variant.clone(),
                        default: arm.default,
                        binding: arm.binding.clone(),
                        body: borrow_call_arg_to_calls(
                            &arm.body,
                            target_name,
                            param_index,
                            region,
                            mutable,
                        )?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        },
    })
}

fn normalize_param_refs(expr: &RawExpr, local_params: &[String]) -> RawExpr {
    normalize_param_refs_scoped(expr, local_params, &mut Vec::new())
}

fn normalize_param_refs_scoped(
    expr: &RawExpr,
    local_params: &[String],
    local_bindings: &mut Vec<String>,
) -> RawExpr {
    match expr {
        RawExpr::LiteralI64 { value } => RawExpr::LiteralI64 {
            value: value.clone(),
        },
        RawExpr::LiteralBool { value } => RawExpr::LiteralBool { value: *value },
        RawExpr::LiteralString { value } => RawExpr::LiteralString {
            value: value.clone(),
        },
        RawExpr::LiteralBytes { bytes_hex } => RawExpr::LiteralBytes {
            bytes_hex: bytes_hex.clone(),
        },
        RawExpr::Unit => RawExpr::Unit,
        RawExpr::ParamRef { index } => RawExpr::ParamRef { index: *index },
        RawExpr::ParamName { name } => {
            if local_bindings.iter().rev().any(|binding| binding == name) {
                RawExpr::ParamName { name: name.clone() }
            } else {
                local_params
                    .iter()
                    .position(|candidate| candidate == name)
                    .map(|index| RawExpr::ParamRef { index })
                    .unwrap_or_else(|| RawExpr::ParamName { name: name.clone() })
            }
        }
        RawExpr::Call { name, args } => RawExpr::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| normalize_param_refs_scoped(arg, local_params, local_bindings))
                .collect(),
        },
        RawExpr::Binary { op, left, right } => RawExpr::Binary {
            op: op.clone(),
            left: Box::new(normalize_param_refs_scoped(
                left,
                local_params,
                local_bindings,
            )),
            right: Box::new(normalize_param_refs_scoped(
                right,
                local_params,
                local_bindings,
            )),
        },
        RawExpr::Unary { op, expr } => RawExpr::Unary {
            op: op.clone(),
            expr: Box::new(normalize_param_refs_scoped(
                expr,
                local_params,
                local_bindings,
            )),
        },
        RawExpr::BorrowShared { region, target } => RawExpr::BorrowShared {
            region: region.clone(),
            target: Box::new(normalize_param_refs_scoped(
                target,
                local_params,
                local_bindings,
            )),
        },
        RawExpr::BorrowMut { region, target } => RawExpr::BorrowMut {
            region: region.clone(),
            target: Box::new(normalize_param_refs_scoped(
                target,
                local_params,
                local_bindings,
            )),
        },
        RawExpr::Assign { target, value } => RawExpr::Assign {
            target: Box::new(normalize_param_refs_scoped(
                target,
                local_params,
                local_bindings,
            )),
            value: Box::new(normalize_param_refs_scoped(
                value,
                local_params,
                local_bindings,
            )),
        },
        RawExpr::Let {
            name,
            ty,
            value,
            body,
        } => {
            let value = normalize_param_refs_scoped(value, local_params, local_bindings);
            local_bindings.push(name.clone());
            let body = normalize_param_refs_scoped(body, local_params, local_bindings);
            local_bindings.pop();
            RawExpr::Let {
                name: name.clone(),
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
            cond: Box::new(normalize_param_refs_scoped(
                cond,
                local_params,
                local_bindings,
            )),
            then_expr: Box::new(normalize_param_refs_scoped(
                then_expr,
                local_params,
                local_bindings,
            )),
            else_expr: Box::new(normalize_param_refs_scoped(
                else_expr,
                local_params,
                local_bindings,
            )),
        },
        RawExpr::Fold {
            item,
            target,
            acc,
            init,
            body,
        } => {
            let target = normalize_param_refs_scoped(target, local_params, local_bindings);
            let init = normalize_param_refs_scoped(init, local_params, local_bindings);
            local_bindings.push(item.clone());
            local_bindings.push(acc.clone());
            let body = normalize_param_refs_scoped(body, local_params, local_bindings);
            local_bindings.pop();
            local_bindings.pop();
            RawExpr::Fold {
                item: item.clone(),
                target: Box::new(target),
                acc: acc.clone(),
                init: Box::new(init),
                body: Box::new(body),
            }
        }
        RawExpr::Array { elements } => RawExpr::Array {
            elements: elements
                .iter()
                .map(|element| normalize_param_refs_scoped(element, local_params, local_bindings))
                .collect(),
        },
        RawExpr::Index { target, index } => RawExpr::Index {
            target: Box::new(normalize_param_refs_scoped(
                target,
                local_params,
                local_bindings,
            )),
            index: Box::new(normalize_param_refs_scoped(
                index,
                local_params,
                local_bindings,
            )),
        },
        RawExpr::Record { fields } => RawExpr::Record {
            fields: fields
                .iter()
                .map(|field| crate::expr::RawRecordField {
                    name: field.name.clone(),
                    value: normalize_param_refs_scoped(&field.value, local_params, local_bindings),
                })
                .collect(),
        },
        RawExpr::FieldAccess { target, field } => RawExpr::FieldAccess {
            target: Box::new(normalize_param_refs_scoped(
                target,
                local_params,
                local_bindings,
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
            value: Box::new(normalize_param_refs_scoped(
                value,
                local_params,
                local_bindings,
            )),
        },
        RawExpr::Case { expr, arms } => {
            let expr = normalize_param_refs_scoped(expr, local_params, local_bindings);
            let arms = arms
                .iter()
                .map(|arm| {
                    if let Some(binding) = &arm.binding {
                        local_bindings.push(binding.clone());
                    }
                    let body = normalize_param_refs_scoped(&arm.body, local_params, local_bindings);
                    if arm.binding.is_some() {
                        local_bindings.pop();
                    }
                    crate::expr::RawCaseArm {
                        variant: arm.variant.clone(),
                        default: arm.default,
                        binding: arm.binding.clone(),
                        body,
                    }
                })
                .collect::<Vec<_>>();
            RawExpr::Case {
                expr: Box::new(expr),
                arms,
            }
        }
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
