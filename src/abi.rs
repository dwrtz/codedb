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
    Ok(())
}

pub(crate) fn exported_abi_names(root: &ProgramRootPayload, symbol: &str) -> Vec<String> {
    exports_for(root, symbol).into_iter().collect()
}

pub(crate) fn export_map(root: &ProgramRootPayload) -> Result<Vec<AbiExport>> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
