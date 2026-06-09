//! The determinism oracle.
//!
//! V3's self-hosting rule is that a CodeDB-hosted pipeline stage is self-hosted
//! only when its artifact reproduces the trusted Rust stage's artifact exactly —
//! hash-for-hash or byte-for-byte (SPEC_V3 §1, §5). Every ladder rung has a
//! deterministic, comparable artifact at its seam:
//!
//! - Rung A (front-end -> lowered IR): IR-hash equality  -> [`assert_hash_identical`]
//! - Rung B (native object emission):  byte-identical `.o` -> [`assert_bytes_identical`]
//! - Rung C (link plan):               identical JSON      -> [`assert_json_identical`]
//!
//! This module is the one helper every rung shares, so "identical" means exactly
//! what the object store means by it: JSON is compared through the same
//! [`canonical_json`] the store hashes with, and byte hashing uses the same
//! [`hash_bytes`]/`BYTES_DOMAIN`. A mismatch returns a structured
//! [`OracleMismatch`] (not a panic) that localizes the first difference, so the
//! helper is usable from tests today and from a future `oracle-check` CLI command.
//!
//! It is intentionally not yet wired into a ladder runner — the rungs themselves
//! arrive in later v3 phases (Phase 8+). This is the substrate they build on.

use serde_json::Value as JsonValue;

use crate::BYTES_DOMAIN;
use crate::store::{canonical_json, hash_bytes};

/// A determinism-oracle comparison failure: the self-hosted stage's artifact did
/// not reproduce the reference stage's artifact at `rung`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleMismatch {
    /// Which ladder seam this comparison guards, e.g. `"ir-hash"`, `"object-bytes"`.
    pub rung: String,
    /// Label for the left/reference artifact (e.g. `"rust-stage"`).
    pub left_label: String,
    /// Label for the right/self-hosted artifact (e.g. `"codedb-stage"`).
    pub right_label: String,
    /// A deterministic, localized description of the first difference.
    pub detail: String,
}

impl std::fmt::Display for OracleMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "determinism oracle [{}] mismatch ({} vs {}): {}",
            self.rung, self.left_label, self.right_label, self.detail
        )
    }
}

impl std::error::Error for OracleMismatch {}

/// Result of an oracle comparison. `Ok(())` means the artifacts are identical
/// under the rung's notion of identity; `Err` localizes the difference.
pub type OracleResult = Result<(), OracleMismatch>;

fn mismatch(rung: &str, left: &str, right: &str, detail: String) -> OracleMismatch {
    OracleMismatch {
        rung: rung.to_string(),
        left_label: left.to_string(),
        right_label: right.to_string(),
        detail,
    }
}

/// Byte-for-byte identity — the Rung B oracle for emitted `.o` artifacts. Each
/// side is `(label, bytes)`. On mismatch the detail reports both lengths and the
/// first differing offset with a short hex window from each side.
pub fn assert_bytes_identical(rung: &str, left: (&str, &[u8]), right: (&str, &[u8])) -> OracleResult {
    if left.1 == right.1 {
        return Ok(());
    }
    Err(mismatch(
        rung,
        left.0,
        right.0,
        byte_diff_summary(left.1, right.1),
    ))
}

/// Hash identity — the Rung A oracle for IR-hash / object-hash comparisons (and
/// any artifact already reduced to a content hash string). Each side is
/// `(label, hash)`.
pub fn assert_hash_identical(rung: &str, left: (&str, &str), right: (&str, &str)) -> OracleResult {
    if left.1 == right.1 {
        return Ok(());
    }
    Err(mismatch(
        rung,
        left.0,
        right.0,
        format!("hash {} != {}", left.1, right.1),
    ))
}

/// Canonical-JSON identity — the Rung C oracle for link-plan / build-plan JSON.
/// Both sides are normalized through the store's [`canonical_json`] before
/// comparison, so two values that differ only in key order or whitespace are
/// treated as identical (that is the determinism contract). On mismatch the
/// detail localizes the first differing JSON path.
pub fn assert_json_identical(
    rung: &str,
    left: (&str, &JsonValue),
    right: (&str, &JsonValue),
) -> OracleResult {
    if canonical_json(left.1) == canonical_json(right.1) {
        return Ok(());
    }
    let detail = json_diff_path(left.1, right.1, "")
        .unwrap_or_else(|| "values differ under canonical encoding".to_string());
    Err(mismatch(rung, left.0, right.0, detail))
}

/// Hash a byte artifact with the store's `BYTES_DOMAIN`, so a caller holding two
/// large byte blobs can compare them by hash exactly as the store would
/// (`assert_hash_identical(rung, (l, &bytes_oracle_hash(a)), (r, &bytes_oracle_hash(b)))`).
pub fn bytes_oracle_hash(bytes: &[u8]) -> String {
    hash_bytes(BYTES_DOMAIN, bytes)
}

fn byte_diff_summary(left: &[u8], right: &[u8]) -> String {
    match left.iter().zip(right).position(|(a, b)| a != b) {
        Some(offset) => format!(
            "{} vs {} bytes; first differ at offset {offset}: {} vs {}",
            left.len(),
            right.len(),
            hex_window(left, offset),
            hex_window(right, offset),
        ),
        None => format!(
            "{} vs {} bytes; common prefix identical, lengths differ",
            left.len(),
            right.len()
        ),
    }
}

fn hex_window(bytes: &[u8], offset: usize) -> String {
    let end = bytes.len().min(offset + 8);
    hex::encode(&bytes[offset..end])
}

/// Walk two JSON values in a deterministic (sorted-key) order and return a
/// description of the first place they differ, or `None` if structurally equal.
fn json_diff_path(left: &JsonValue, right: &JsonValue, path: &str) -> Option<String> {
    match (left, right) {
        (JsonValue::Object(left_map), JsonValue::Object(right_map)) => {
            let mut keys = left_map.keys().chain(right_map.keys()).collect::<Vec<_>>();
            keys.sort();
            keys.dedup();
            for key in keys {
                let child = format!("{path}/{key}");
                match (left_map.get(key), right_map.get(key)) {
                    (Some(left_value), Some(right_value)) => {
                        if let Some(detail) = json_diff_path(left_value, right_value, &child) {
                            return Some(detail);
                        }
                    }
                    (Some(_), None) => return Some(format!("{child} present on left only")),
                    (None, Some(_)) => return Some(format!("{child} present on right only")),
                    (None, None) => unreachable!(),
                }
            }
            None
        }
        (JsonValue::Array(left_values), JsonValue::Array(right_values)) => {
            if left_values.len() != right_values.len() {
                return Some(format!(
                    "{path} array length {} vs {}",
                    left_values.len(),
                    right_values.len()
                ));
            }
            for (index, (left_value, right_value)) in
                left_values.iter().zip(right_values).enumerate()
            {
                let child = format!("{path}/{index}");
                if let Some(detail) = json_diff_path(left_value, right_value, &child) {
                    return Some(detail);
                }
            }
            None
        }
        _ => {
            if canonical_json(left) == canonical_json(right) {
                None
            } else {
                let location = if path.is_empty() { "/" } else { path };
                Some(format!(
                    "{location}: {} vs {}",
                    truncate(left),
                    truncate(right)
                ))
            }
        }
    }
}

fn truncate(value: &JsonValue) -> String {
    let rendered = canonical_json(value);
    if rendered.len() > 60 {
        format!("{}…", &rendered[..60])
    } else {
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_bytes_pass() {
        assert!(assert_bytes_identical("object-bytes", ("rust", b"\x01\x02\x03"), ("codedb", b"\x01\x02\x03")).is_ok());
    }

    #[test]
    fn differing_bytes_localize_first_offset() {
        let err = assert_bytes_identical("object-bytes", ("rust", b"\xaa\xbb\xcc\xdd"), ("codedb", b"\xaa\xbb\x00\xdd"))
            .unwrap_err();
        assert_eq!(err.rung, "object-bytes");
        assert_eq!(err.left_label, "rust");
        assert_eq!(err.right_label, "codedb");
        assert!(err.detail.contains("offset 2"), "detail: {}", err.detail);
        assert!(err.detail.contains("cc"), "detail: {}", err.detail);
    }

    #[test]
    fn length_mismatch_with_common_prefix() {
        let err = assert_bytes_identical("object-bytes", ("a", b"\x01\x02"), ("b", b"\x01\x02\x03"))
            .unwrap_err();
        assert!(err.detail.contains("lengths differ"), "detail: {}", err.detail);
    }

    #[test]
    fn identical_hash_pass_and_diff_fail() {
        assert!(assert_hash_identical("ir-hash", ("a", "sha256:abc"), ("b", "sha256:abc")).is_ok());
        let err = assert_hash_identical("ir-hash", ("a", "sha256:abc"), ("b", "sha256:def")).unwrap_err();
        assert!(err.detail.contains("sha256:abc"));
        assert!(err.detail.contains("sha256:def"));
    }

    #[test]
    fn json_equality_ignores_key_order() {
        let left = json!({"x": 1, "y": [1, 2, 3], "z": {"a": true}});
        let right = json!({"z": {"a": true}, "y": [1, 2, 3], "x": 1});
        assert!(assert_json_identical("link-plan", ("rust", &left), ("codedb", &right)).is_ok());
    }

    #[test]
    fn json_diff_localizes_nested_path() {
        let left = json!({"outer": {"inner": 1}});
        let right = json!({"outer": {"inner": 2}});
        let err = assert_json_identical("link-plan", ("rust", &left), ("codedb", &right)).unwrap_err();
        assert!(err.detail.contains("/outer/inner"), "detail: {}", err.detail);
    }

    #[test]
    fn json_diff_reports_missing_key() {
        let left = json!({"a": 1, "b": 2});
        let right = json!({"a": 1});
        let err = assert_json_identical("link-plan", ("rust", &left), ("codedb", &right)).unwrap_err();
        assert!(err.detail.contains("/b"), "detail: {}", err.detail);
        assert!(err.detail.contains("left only"), "detail: {}", err.detail);
    }

    #[test]
    fn bytes_hash_matches_store_hashing() {
        // Same bytes -> same oracle hash -> hash oracle agrees.
        let a = bytes_oracle_hash(b"hello");
        let b = bytes_oracle_hash(b"hello");
        assert!(assert_hash_identical("object-hash", ("a", &a), ("b", &b)).is_ok());
        let c = bytes_oracle_hash(b"world");
        assert!(assert_hash_identical("object-hash", ("a", &a), ("c", &c)).is_err());
    }
}
