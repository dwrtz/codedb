use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProgramRootPayload {
    pub(crate) symbols: Vec<RootSymbolPayload>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) types: Vec<RootTypePayload>,
    pub(crate) names: Vec<NameBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) type_names: Vec<TypeNameBinding>,
    pub(crate) param_names: Vec<ParamNames>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) exports: Vec<ExportBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tests: Vec<RootTestBinding>,
    pub(crate) metadata: BTreeMap<String, JsonValue>,
}

pub(crate) const ROOT_MODULES_METADATA_KEY: &str = "modules";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RootSymbolPayload {
    pub(crate) symbol: String,
    pub(crate) definition: String,
    pub(crate) signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RootTypePayload {
    pub(crate) type_symbol: String,
    pub(crate) type_def: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NameBinding {
    pub(crate) module: String,
    pub(crate) display_name: String,
    pub(crate) symbol: String,
    pub(crate) is_preferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TypeNameBinding {
    pub(crate) module: String,
    pub(crate) display_name: String,
    pub(crate) type_symbol: String,
    pub(crate) is_preferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ParamNames {
    pub(crate) symbol: String,
    pub(crate) names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExportBinding {
    pub(crate) symbol: String,
    pub(crate) exported_name: String,
}

pub(crate) const TEST_CASE_SCHEMA_V1: &str = "codedb/test-case/v1";
pub(crate) const TEST_CASE_SCHEMA_V2: &str = "codedb/test-case/v2";
pub(crate) const TEST_CASE_SCHEMA: &str = TEST_CASE_SCHEMA_V2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RootTestBinding {
    pub(crate) name: String,
    pub(crate) test: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TestCasePayload {
    #[serde(default = "default_test_case_schema")]
    pub(crate) schema: String,
    #[serde(default, skip_serializing_if = "TestCategory::is_behavior")]
    pub(crate) category: TestCategory,
    #[serde(default, skip_serializing_if = "TestMode::is_reference")]
    pub(crate) mode: TestMode,
    pub(crate) entry_symbol: String,
    pub(crate) args: Vec<TestValue>,
    pub(crate) expected: TestValue,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) native_agreement: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) native_required: bool,
}

impl TestCasePayload {
    pub(crate) fn native_requested(&self) -> bool {
        self.native_agreement || matches!(self.mode, TestMode::ReferenceAndNative)
    }

    pub(crate) fn labels(&self) -> Vec<&'static str> {
        let mut labels = Vec::new();
        if self.native_required {
            labels.push("v2_native_required");
        }
        labels
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TestCategory {
    #[default]
    Behavior,
    Projection,
    Export,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TestMode {
    #[default]
    Reference,
    ReferenceAndNative,
}

impl TestMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TestMode::Reference => "reference",
            TestMode::ReferenceAndNative => "reference_and_native",
        }
    }

    pub(crate) fn is_reference(&self) -> bool {
        matches!(self, TestMode::Reference)
    }
}

impl TestCategory {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TestCategory::Behavior => "behavior",
            TestCategory::Projection => "projection",
            TestCategory::Export => "export",
        }
    }

    pub(crate) fn is_behavior(&self) -> bool {
        matches!(self, TestCategory::Behavior)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TestValue {
    I64 { value: String },
    Bool { value: bool },
    Unit,
}

#[derive(Debug, Clone)]
pub(crate) struct BranchState {
    pub(crate) root_hash: String,
    pub(crate) history_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TypeCheckResult {
    pub(crate) expr_hash: String,
    pub(crate) type_hash: String,
}

pub(crate) fn normalize_root(mut root: ProgramRootPayload) -> ProgramRootPayload {
    root.symbols.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    root.types.sort_by(|a, b| a.type_symbol.cmp(&b.type_symbol));
    root.names.sort_by(|a, b| {
        (&a.module, &a.display_name, &a.symbol, a.is_preferred).cmp(&(
            &b.module,
            &b.display_name,
            &b.symbol,
            b.is_preferred,
        ))
    });
    root.type_names.sort_by(|a, b| {
        (&a.module, &a.display_name, &a.type_symbol, a.is_preferred).cmp(&(
            &b.module,
            &b.display_name,
            &b.type_symbol,
            b.is_preferred,
        ))
    });
    root.param_names.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    root.exports
        .sort_by(|a, b| (&a.exported_name, &a.symbol).cmp(&(&b.exported_name, &b.symbol)));
    root.tests
        .sort_by(|a, b| (&a.name, &a.test).cmp(&(&b.name, &b.test)));
    root
}

pub(crate) fn root_module_names(root: &ProgramRootPayload) -> BTreeSet<String> {
    let mut modules = root
        .metadata
        .get(ROOT_MODULES_METADATA_KEY)
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("name").and_then(JsonValue::as_str))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    modules.extend(root.names.iter().map(|binding| binding.module.clone()));
    modules.extend(root.type_names.iter().map(|binding| binding.module.clone()));
    if modules.is_empty() {
        modules.insert("main".to_string());
    }
    modules
}

pub(crate) fn synchronize_module_metadata(root: &mut ProgramRootPayload) {
    let modules = root
        .names
        .iter()
        .map(|binding| binding.module.clone())
        .chain(root.type_names.iter().map(|binding| binding.module.clone()))
        .collect::<BTreeSet<_>>();
    if modules.is_empty() {
        root.metadata.remove(ROOT_MODULES_METADATA_KEY);
        return;
    }
    root.metadata.insert(
        ROOT_MODULES_METADATA_KEY.to_string(),
        JsonValue::Array(
            modules
                .into_iter()
                .map(|name| {
                    let mut object = serde_json::Map::new();
                    object.insert("name".to_string(), JsonValue::String(name));
                    JsonValue::Object(object)
                })
                .collect(),
        ),
    );
}

pub(crate) fn root_symbol_index(root: &ProgramRootPayload, symbol: &str) -> Result<usize> {
    root.symbols
        .iter()
        .position(|entry| entry.symbol == symbol)
        .ok_or_else(|| anyhow!("symbol missing from root {symbol}"))
}

pub(crate) fn root_type_index(root: &ProgramRootPayload, type_symbol: &str) -> Result<usize> {
    root.types
        .iter()
        .position(|entry| entry.type_symbol == type_symbol)
        .ok_or_else(|| anyhow!("type missing from root {type_symbol}"))
}

pub(crate) fn upsert_param_names(root: &mut ProgramRootPayload, symbol: &str, names: Vec<String>) {
    if let Some(entry) = root
        .param_names
        .iter_mut()
        .find(|entry| entry.symbol == symbol)
    {
        entry.names = names;
    } else {
        root.param_names.push(ParamNames {
            symbol: symbol.to_string(),
            names,
        });
    }
}

pub(crate) fn param_names(root: &ProgramRootPayload, symbol: &str) -> Vec<String> {
    root.param_names
        .iter()
        .find(|entry| entry.symbol == symbol)
        .map(|entry| entry.names.clone())
        .unwrap_or_default()
}

pub(crate) fn preferred_names(root: &ProgramRootPayload) -> Vec<NameBinding> {
    let mut names = root
        .names
        .iter()
        .filter(|binding| binding.is_preferred)
        .cloned()
        .collect::<Vec<_>>();
    names.sort_by(|a, b| {
        (&a.module, &a.display_name, &a.symbol).cmp(&(&b.module, &b.display_name, &b.symbol))
    });
    names
}

pub(crate) fn preferred_type_names(root: &ProgramRootPayload) -> Vec<TypeNameBinding> {
    let mut names = root
        .type_names
        .iter()
        .filter(|binding| binding.is_preferred)
        .cloned()
        .collect::<Vec<_>>();
    names.sort_by(|a, b| {
        (&a.module, &a.display_name, &a.type_symbol).cmp(&(
            &b.module,
            &b.display_name,
            &b.type_symbol,
        ))
    });
    names
}

pub(crate) fn preferred_binding<'a>(
    root: &'a ProgramRootPayload,
    symbol: &str,
) -> Option<&'a NameBinding> {
    root.names
        .iter()
        .find(|binding| binding.symbol == symbol && binding.is_preferred)
        .or_else(|| root.names.iter().find(|binding| binding.symbol == symbol))
}

pub(crate) fn preferred_type_binding<'a>(
    root: &'a ProgramRootPayload,
    type_symbol: &str,
) -> Option<&'a TypeNameBinding> {
    root.type_names
        .iter()
        .find(|binding| binding.type_symbol == type_symbol && binding.is_preferred)
        .or_else(|| {
            root.type_names
                .iter()
                .find(|binding| binding.type_symbol == type_symbol)
        })
}

pub(crate) fn aliases_for(root: &ProgramRootPayload, symbol: &str) -> BTreeSet<String> {
    root.names
        .iter()
        .filter(|binding| binding.symbol == symbol && !binding.is_preferred)
        .map(|binding| binding.display_name.clone())
        .collect()
}

pub(crate) fn exports_for(root: &ProgramRootPayload, symbol: &str) -> BTreeSet<String> {
    root.exports
        .iter()
        .filter(|binding| binding.symbol == symbol)
        .map(|binding| binding.exported_name.clone())
        .collect()
}

pub(crate) fn test_binding_for<'a>(
    root: &'a ProgramRootPayload,
    name: &str,
) -> Option<&'a RootTestBinding> {
    root.tests.iter().find(|binding| binding.name == name)
}

pub(crate) fn resolve_name_in_root(
    root: &ProgramRootPayload,
    module: &str,
    name: &str,
) -> Option<String> {
    root.names
        .iter()
        .find(|binding| binding.module == module && binding.display_name == name)
        .map(|binding| binding.symbol.clone())
}

pub(crate) fn resolve_type_name_in_root(
    root: &ProgramRootPayload,
    module: &str,
    name: &str,
) -> Option<String> {
    root.type_names
        .iter()
        .find(|binding| binding.module == module && binding.display_name == name)
        .map(|binding| binding.type_symbol.clone())
}

pub(crate) fn split_qualified_name(name: &str) -> Option<(&str, &str)> {
    let (module, local_name) = name.rsplit_once('.')?;
    if module.is_empty() || local_name.is_empty() {
        return None;
    }
    Some((module, local_name))
}

pub(crate) fn resolve_function_name_in_root(
    root: &ProgramRootPayload,
    current_module: &str,
    name: &str,
) -> Option<String> {
    if let Some((module, local_name)) = split_qualified_name(name) {
        return resolve_name_in_root(root, module, local_name);
    }
    resolve_name_in_root(root, current_module, name)
}

pub(crate) fn resolve_named_type_in_root(
    root: &ProgramRootPayload,
    current_module: &str,
    name: &str,
) -> Option<String> {
    if let Some((module, local_name)) = split_qualified_name(name) {
        return resolve_type_name_in_root(root, module, local_name);
    }
    resolve_type_name_in_root(root, current_module, name)
}

pub(crate) fn symbol_display_in_module(
    root: &ProgramRootPayload,
    current_module: &str,
    symbol: &str,
) -> Option<String> {
    let binding = preferred_binding(root, symbol)?;
    if binding.module == current_module {
        Some(binding.display_name.clone())
    } else {
        Some(format!("{}.{}", binding.module, binding.display_name))
    }
}

pub(crate) fn qualified_symbol_display(root: &ProgramRootPayload, symbol: &str) -> Option<String> {
    let binding = preferred_binding(root, symbol)?;
    Some(format!("{}.{}", binding.module, binding.display_name))
}

pub(crate) fn type_symbol_display_in_module(
    root: &ProgramRootPayload,
    current_module: &str,
    type_symbol: &str,
) -> Option<String> {
    let binding = preferred_type_binding(root, type_symbol)?;
    if binding.module == current_module {
        Some(binding.display_name.clone())
    } else {
        Some(format!("{}.{}", binding.module, binding.display_name))
    }
}

pub(crate) fn validate_projection_identifier(label: &str, name: &str) -> Result<()> {
    if !is_projection_identifier(name) {
        anyhow::bail!("{label} must be a projection-safe identifier: {name:?}");
    }
    Ok(())
}

pub(crate) fn validate_module_path(label: &str, module: &str) -> Result<()> {
    if module.is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    for segment in module.split('.') {
        validate_projection_identifier(label, segment)?;
    }
    Ok(())
}

pub(crate) fn is_projection_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '_' && !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) && !reserved_projection_identifier(name)
}

fn reserved_projection_identifier(name: &str) -> bool {
    matches!(
        name,
        "fn" | "if"
            | "then"
            | "else"
            | "let"
            | "in"
            | "module"
            | "mut"
            | "of"
            | "record"
            | "true"
            | "false"
            | "i64"
            | "bool"
            | "unit"
            | "auto"
            | "break"
            | "case"
            | "char"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "enum"
            | "extern"
            | "float"
            | "for"
            | "goto"
            | "int"
            | "long"
            | "register"
            | "return"
            | "short"
            | "signed"
            | "sizeof"
            | "static"
            | "struct"
            | "switch"
            | "typedef"
            | "union"
            | "unsigned"
            | "void"
            | "volatile"
            | "while"
    )
}

fn default_test_case_schema() -> String {
    TEST_CASE_SCHEMA.to_string()
}

fn is_false(value: &bool) -> bool {
    !*value
}
