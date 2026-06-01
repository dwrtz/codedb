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
