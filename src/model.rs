use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProgramRootPayload {
    pub(crate) symbols: Vec<RootSymbolPayload>,
    pub(crate) names: Vec<NameBinding>,
    pub(crate) param_names: Vec<ParamNames>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) exports: Vec<ExportBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tests: Vec<RootTestBinding>,
    pub(crate) metadata: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RootSymbolPayload {
    pub(crate) symbol: String,
    pub(crate) definition: String,
    pub(crate) signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NameBinding {
    pub(crate) module: String,
    pub(crate) display_name: String,
    pub(crate) symbol: String,
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

pub(crate) const TEST_CASE_SCHEMA: &str = "codedb/test-case/v1";

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
    pub(crate) entry_symbol: String,
    pub(crate) args: Vec<TestValue>,
    pub(crate) expected: TestValue,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) native_agreement: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TestCategory {
    Behavior,
    Projection,
    Export,
}

impl Default for TestCategory {
    fn default() -> Self {
        Self::Behavior
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
    root.names.sort_by(|a, b| {
        (&a.module, &a.display_name, &a.symbol, a.is_preferred).cmp(&(
            &b.module,
            &b.display_name,
            &b.symbol,
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

pub(crate) fn root_symbol_index(root: &ProgramRootPayload, symbol: &str) -> Result<usize> {
    root.symbols
        .iter()
        .position(|entry| entry.symbol == symbol)
        .ok_or_else(|| anyhow!("symbol missing from root {symbol}"))
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

pub(crate) fn validate_projection_identifier(label: &str, name: &str) -> Result<()> {
    if !is_projection_identifier(name) {
        anyhow::bail!("{label} must be a projection-safe identifier: {name:?}");
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
