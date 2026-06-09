use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::{ProgramRootPayload, exports_for};

pub(crate) const INTERNAL_ABI_PREFIX: &str = "codedb_";
pub(crate) const INTERNAL_ABI_HASH_HEX_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AbiExport {
    pub(crate) symbol: String,
    pub(crate) internal_abi_symbol: String,
    pub(crate) exported_name: String,
}

pub(crate) fn internal_abi_symbol(symbol_hash: &str) -> Result<String> {
    let Some(hex) = symbol_hash.strip_prefix("sha256:") else {
        bail!("symbol hash must use sha256: prefix");
    };
    if hex.len() < INTERNAL_ABI_HASH_HEX_LEN || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("symbol hash must contain at least {INTERNAL_ABI_HASH_HEX_LEN} hex characters");
    }
    Ok(format!(
        "{INTERNAL_ABI_PREFIX}{}",
        &hex[..INTERNAL_ABI_HASH_HEX_LEN]
    ))
}

pub(crate) fn validate_exported_abi_name(name: &str) -> Result<()> {
    if !is_valid_abi_identifier(name) {
        bail!("bad ABI export name {name:?}");
    }
    if is_reserved_native_export_name(name) {
        bail!("reserved native ABI export name {name:?}");
    }
    Ok(())
}

pub(crate) fn exported_abi_names(root: &ProgramRootPayload, symbol: &str) -> Vec<String> {
    exports_for(root, symbol).into_iter().collect()
}

pub(crate) fn validate_export_map(root: &ProgramRootPayload) -> Result<()> {
    let root_symbols = root
        .symbols
        .iter()
        .map(|entry| entry.symbol.clone())
        .collect::<BTreeSet<_>>();
    let mut internal_symbols = BTreeMap::new();
    for entry in &root.symbols {
        let internal = internal_abi_symbol(&entry.symbol)?;
        if let Some(existing) = internal_symbols.insert(internal.clone(), entry.symbol.clone()) {
            bail!(
                "internal ABI symbol {internal} is shared by symbols {existing} and {}",
                entry.symbol
            );
        }
    }

    let mut exported_names = BTreeSet::new();
    for binding in &root.exports {
        if !root_symbols.contains(&binding.symbol) {
            bail!(
                "export {} points to missing symbol {}",
                binding.exported_name,
                binding.symbol
            );
        }
        validate_exported_abi_name(&binding.exported_name)?;
        if !exported_names.insert(binding.exported_name.clone()) {
            bail!("duplicate exported ABI name {}", binding.exported_name);
        }
        if let Some(owner) = internal_symbols.get(&binding.exported_name)
            && owner != &binding.symbol
        {
            bail!(
                "exported ABI name {} conflicts with internal ABI symbol for {}",
                binding.exported_name,
                owner
            );
        }
    }
    Ok(())
}

pub(crate) fn export_map(root: &ProgramRootPayload) -> Result<Vec<AbiExport>> {
    validate_export_map(root)?;
    root.exports
        .iter()
        .map(|binding| {
            Ok(AbiExport {
                symbol: binding.symbol.clone(),
                internal_abi_symbol: internal_abi_symbol(&binding.symbol)?,
                exported_name: binding.exported_name.clone(),
            })
        })
        .collect()
}

fn is_valid_abi_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '_' && !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn is_reserved_native_export_name(name: &str) -> bool {
    name == "main" || is_c_keyword(name)
}

fn is_c_keyword(name: &str) -> bool {
    matches!(
        name,
        "auto"
            | "break"
            | "case"
            | "char"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extern"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "inline"
            | "int"
            | "long"
            | "register"
            | "restrict"
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
            | "_Alignas"
            | "_Alignof"
            | "_Atomic"
            | "_Bool"
            | "_Complex"
            | "_Generic"
            | "_Imaginary"
            | "_Noreturn"
            | "_Static_assert"
            | "_Thread_local"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExportBinding, RootSymbolPayload};

    #[test]
    fn internal_abi_symbol_uses_stable_hash_prefix() {
        let symbol = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            internal_abi_symbol(symbol).unwrap(),
            "codedb_0123456789abcdef"
        );
    }

    #[test]
    fn exported_abi_names_are_plain_identifiers() {
        validate_exported_abi_name("public_tax").unwrap();
        validate_exported_abi_name("_start").unwrap();
        assert!(validate_exported_abi_name("9tax").is_err());
        assert!(validate_exported_abi_name("sales-tax").is_err());
        assert!(validate_exported_abi_name("main").is_err());
        assert!(validate_exported_abi_name("long").is_err());
    }

    #[test]
    fn export_map_rejects_names_that_collide_with_other_internal_symbols() {
        let first = symbol_hash("1111111111111111");
        let second = symbol_hash("2222222222222222");
        let root = ProgramRootPayload {
            symbols: vec![root_symbol(&first), root_symbol(&second)],
            types: vec![],
            names: vec![],
            type_names: vec![],
            param_names: vec![],
            exports: vec![ExportBinding {
                symbol: first,
                exported_name: internal_abi_symbol(&second).unwrap(),
            }],
            tests: vec![],
            recursion_groups: vec![],
            type_recursion_groups: vec![],
            metadata: Default::default(),
        };

        assert!(validate_export_map(&root).is_err());
    }

    fn root_symbol(symbol: &str) -> RootSymbolPayload {
        RootSymbolPayload {
            symbol: symbol.to_string(),
            definition: symbol_hash("aaaaaaaaaaaaaaaa"),
            signature: symbol_hash("bbbbbbbbbbbbbbbb"),
        }
    }

    fn symbol_hash(prefix: &str) -> String {
        format!("sha256:{prefix}{}", "0".repeat(64 - prefix.len()))
    }
}
