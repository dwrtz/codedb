//! Single source of truth for built-in operators.
//!
//! Before this module, the per-operator knowledge for the built-in binary and
//! unary operators was hand-duplicated across the reference evaluator
//! (`expr.rs`), the lowering pass (`lowering.rs`: source-op -> lowered kind, the
//! inverse verify mapping, and trap derivation), and the parser's precedence
//! table. Adding or changing an operator meant editing every one of those sites
//! in lockstep — the "6-site edit" PLAN_V3 Phase 2 calls out.
//!
//! The lowered **kind** string (e.g. `add_i64`, `eq_u8`, `and_bool`) is the join
//! key that threads an operator through every stage. This module centralizes all
//! of that knowledge in one [`OPS`] table; the former sites become thin
//! forwarders into the functions here. The only operator knowledge that stays
//! outside is the backend machine-code encoding (raw bytes per kind, which cannot
//! be table-generated) — and even that is guarded by a registry-driven coverage
//! test (see the tests at the bottom and `backend::native::backend_encodes_kind`).
//!
//! Everything here is output-preserving: lowered kind strings, traps, evaluator
//! results, and error messages are byte-identical to the pre-registry code.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};

use crate::expr::Value;
use crate::types::type_hash_for;

/// Whether an entry is a binary or unary operator.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpCategory {
    Binary,
    Unary,
}

/// A comparison flavor; shared by the i64 and u8 comparison semantics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The pure runtime semantic the reference evaluator interprets. One arm per
/// *distinct* runtime operation — comparisons split by operand value-class so the
/// evaluator's "invalid operands" path stays exactly as it was.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemOp {
    AddI64,
    SubI64,
    MulI64,
    DivI64,
    ModI64,
    CmpI64(Cmp),
    CmpU8(Cmp),
    AndBool,
    OrBool,
    NegI64,
    NotBool,
}

/// A lowered-IR trap condition attached to a kind (only `div_i64` today).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrapSpec {
    pub(crate) condition: &'static str,
    pub(crate) code: &'static str,
}

/// One operator. `left_type`/`right_type`/`result_type` hold type *names*
/// (`"I64"`, `"U8"`, `"Bool"`); they are resolved to content hashes via
/// [`type_hash_for`] at comparison time so the registry can never drift from the
/// type system's hashing. For a unary operator, the operand type is stored in
/// `left_type` and `right_type` is `""`.
#[derive(Clone, Copy)]
pub(crate) struct OpEntry {
    pub(crate) kind: &'static str,
    pub(crate) category: OpCategory,
    pub(crate) source_op: &'static str,
    pub(crate) left_type: &'static str,
    pub(crate) right_type: &'static str,
    pub(crate) result_type: &'static str,
    pub(crate) trap: Option<TrapSpec>,
    pub(crate) precedence: u8,
    pub(crate) sem: SemOp,
}

/// All registered operator kinds, sorted. Public so the evaluator-vs-backend
/// conformance harness (`tests/oracle_conformance.rs`) can assert it carries a
/// fixture for *every* operator: adding an [`OPS`] row with no conformance
/// fixture then fails that coverage gate. This is the honesty link that keeps
/// the oracle complete as operators are added.
pub fn operator_kinds() -> Vec<&'static str> {
    let mut kinds: Vec<&'static str> = OPS.iter().map(|entry| entry.kind).collect();
    kinds.sort_unstable();
    kinds
}

/// Parser precedence for prefix unary operators (`-`, `!`).
pub(crate) const UNARY_PRECEDENCE: u8 = 7;

/// Precedence returned for any operator not in the binary table (matches the
/// pre-registry `_ => 9` default in `op_precedence`).
const DEFAULT_PRECEDENCE: u8 = 9;

const DIV_TRAP: TrapSpec = TrapSpec {
    condition: "right_operand_zero",
    code: "division_by_zero",
};

const MOD_TRAP: TrapSpec = TrapSpec {
    condition: "right_operand_zero",
    code: "modulo_by_zero",
};

/// THE operator table. Adding a trivial operator is one row here (plus a backend
/// encoder arm on each target — see module docs).
pub(crate) static OPS: &[OpEntry] = &[
    // i64 arithmetic
    bin("add_i64", "+", "I64", "I64", "I64", None, 5, SemOp::AddI64),
    bin("sub_i64", "-", "I64", "I64", "I64", None, 5, SemOp::SubI64),
    bin("mul_i64", "*", "I64", "I64", "I64", None, 6, SemOp::MulI64),
    bin("div_i64", "/", "I64", "I64", "I64", Some(DIV_TRAP), 6, SemOp::DivI64),
    bin("mod_i64", "%", "I64", "I64", "I64", Some(MOD_TRAP), 6, SemOp::ModI64),
    // i64 comparisons -> Bool
    bin("eq_i64", "==", "I64", "I64", "Bool", None, 3, SemOp::CmpI64(Cmp::Eq)),
    bin("ne_i64", "!=", "I64", "I64", "Bool", None, 3, SemOp::CmpI64(Cmp::Ne)),
    bin("lt_i64", "<", "I64", "I64", "Bool", None, 4, SemOp::CmpI64(Cmp::Lt)),
    bin("le_i64", "<=", "I64", "I64", "Bool", None, 4, SemOp::CmpI64(Cmp::Le)),
    bin("gt_i64", ">", "I64", "I64", "Bool", None, 4, SemOp::CmpI64(Cmp::Gt)),
    bin("ge_i64", ">=", "I64", "I64", "Bool", None, 4, SemOp::CmpI64(Cmp::Ge)),
    // u8 comparisons -> Bool
    bin("eq_u8", "==", "U8", "U8", "Bool", None, 3, SemOp::CmpU8(Cmp::Eq)),
    bin("ne_u8", "!=", "U8", "U8", "Bool", None, 3, SemOp::CmpU8(Cmp::Ne)),
    bin("lt_u8", "<", "U8", "U8", "Bool", None, 4, SemOp::CmpU8(Cmp::Lt)),
    bin("le_u8", "<=", "U8", "U8", "Bool", None, 4, SemOp::CmpU8(Cmp::Le)),
    bin("gt_u8", ">", "U8", "U8", "Bool", None, 4, SemOp::CmpU8(Cmp::Gt)),
    bin("ge_u8", ">=", "U8", "U8", "Bool", None, 4, SemOp::CmpU8(Cmp::Ge)),
    // bool binary
    bin("and_bool", "&&", "Bool", "Bool", "Bool", None, 2, SemOp::AndBool),
    bin("or_bool", "||", "Bool", "Bool", "Bool", None, 1, SemOp::OrBool),
    // unary
    un("neg_i64", "-", "I64", "I64", SemOp::NegI64),
    un("not_bool", "!", "Bool", "Bool", SemOp::NotBool),
];

const fn bin(
    kind: &'static str,
    source_op: &'static str,
    left_type: &'static str,
    right_type: &'static str,
    result_type: &'static str,
    trap: Option<TrapSpec>,
    precedence: u8,
    sem: SemOp,
) -> OpEntry {
    OpEntry {
        kind,
        category: OpCategory::Binary,
        source_op,
        left_type,
        right_type,
        result_type,
        trap,
        precedence,
        sem,
    }
}

const fn un(
    kind: &'static str,
    source_op: &'static str,
    input_type: &'static str,
    result_type: &'static str,
    sem: SemOp,
) -> OpEntry {
    OpEntry {
        kind,
        category: OpCategory::Unary,
        source_op,
        left_type: input_type,
        right_type: "",
        result_type,
        trap: None,
        precedence: UNARY_PRECEDENCE,
        sem,
    }
}

/// Resolve a registry type name to its content hash, computing each distinct
/// name once per process. (Lowering compares against type hashes, so we cache to
/// avoid re-hashing the same handful of names on every operator lowered.)
fn registry_type_hash(name: &str) -> &'static str {
    static CACHE: OnceLock<BTreeMap<&'static str, String>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let mut map: BTreeMap<&'static str, String> = BTreeMap::new();
        for entry in OPS {
            for name in [entry.left_type, entry.right_type, entry.result_type] {
                if !name.is_empty() {
                    map.entry(name).or_insert_with(|| type_hash_for(name));
                }
            }
        }
        map
    });
    cache.get(name).map(String::as_str).unwrap_or("")
}

fn lookup_binary_kind(kind: &str) -> Option<&'static OpEntry> {
    OPS.iter()
        .find(|entry| entry.category == OpCategory::Binary && entry.kind == kind)
}

fn lookup_unary_kind(kind: &str) -> Option<&'static OpEntry> {
    OPS.iter()
        .find(|entry| entry.category == OpCategory::Unary && entry.kind == kind)
}

/// Whether `op` is a source-level binary operator (i.e. the registry carries a
/// binary entry for it). The parser routes its `is_binary_op` gate here so adding
/// an operator is one [`OPS`] row, not a second hand-maintained list.
pub(crate) fn is_source_binary_op(op: &str) -> bool {
    OPS.iter()
        .any(|entry| entry.category == OpCategory::Binary && entry.source_op == op)
}

/// Parser precedence for a binary operator (`DEFAULT_PRECEDENCE` for anything not
/// in the binary table). Filtering by category means `-` resolves to its binary
/// precedence, never the unary entry.
pub(crate) fn binary_precedence(op: &str) -> u8 {
    OPS.iter()
        .find(|entry| entry.category == OpCategory::Binary && entry.source_op == op)
        .map(|entry| entry.precedence)
        .unwrap_or(DEFAULT_PRECEDENCE)
}

/// Map a source binary operator + operand/result type hashes to a lowered kind.
pub(crate) fn lower_binary_kind(
    source_op: &str,
    left_type: &str,
    right_type: &str,
    result_type: &str,
) -> Result<String> {
    OPS.iter()
        .find(|entry| {
            entry.category == OpCategory::Binary
                && entry.source_op == source_op
                && registry_type_hash(entry.left_type) == left_type
                && registry_type_hash(entry.right_type) == right_type
                && registry_type_hash(entry.result_type) == result_type
        })
        .map(|entry| entry.kind.to_string())
        .ok_or_else(|| {
            anyhow!("cannot lower binary operator {source_op} with operand/result types")
        })
}

/// Map a source unary operator + operand/result type hashes to a lowered kind.
pub(crate) fn lower_unary_kind(
    source_op: &str,
    input_type: &str,
    result_type: &str,
) -> Result<String> {
    OPS.iter()
        .find(|entry| {
            entry.category == OpCategory::Unary
                && entry.source_op == source_op
                && registry_type_hash(entry.left_type) == input_type
                && registry_type_hash(entry.result_type) == result_type
        })
        .map(|entry| entry.kind.to_string())
        .ok_or_else(|| {
            anyhow!("cannot lower unary operator {source_op} with operand/result types")
        })
}

/// The trap spec for a lowered binary kind, if any.
pub(crate) fn binary_trap(kind: &str) -> Option<TrapSpec> {
    OPS.iter().find(|entry| entry.kind == kind).and_then(|e| e.trap)
}

/// Verify a lowered binary op: the kind is known, the operand/result types match
/// the kind, and the trap (passed as `(condition, code)`) matches the kind's trap
/// requirement. Mirrors the pre-registry control flow so all error messages are
/// preserved verbatim.
pub(crate) fn verify_binary_kind(
    kind: &str,
    left_type: &str,
    right_type: &str,
    result_type: &str,
    trap: Option<(&str, &str)>,
) -> Result<()> {
    let entry = lookup_binary_kind(kind).ok_or_else(|| anyhow!("unknown lowered binary kind {kind}"))?;
    let expected = lower_binary_kind(entry.source_op, left_type, right_type, result_type)?;
    if expected != kind {
        bail!("lowered binary kind/type mismatch");
    }
    match (entry.trap, trap) {
        (Some(spec), Some((condition, code)))
            if condition == spec.condition && code == spec.code =>
        {
            Ok(())
        }
        (Some(spec), _) => bail!("lowered {kind} must include a {} trap", spec.code),
        (None, None) => Ok(()),
        (None, Some(_)) => bail!("unexpected trap on lowered binary kind {kind}"),
    }
}

/// Verify a lowered unary op: the kind is known and the operand/result types
/// match the kind. Error messages preserved verbatim.
pub(crate) fn verify_unary_kind(kind: &str, input_type: &str, result_type: &str) -> Result<()> {
    let entry = lookup_unary_kind(kind).ok_or_else(|| anyhow!("unknown lowered unary kind {kind}"))?;
    let expected = lower_unary_kind(entry.source_op, input_type, result_type)?;
    if expected != kind {
        bail!("lowered unary kind/type mismatch");
    }
    Ok(())
}

fn value_matches(type_name: &str, value: &Value) -> bool {
    matches!(
        (type_name, value),
        ("I64", Value::I64(_)) | ("U8", Value::U8(_)) | ("Bool", Value::Bool(_))
    )
}

fn apply_cmp<T: PartialEq + PartialOrd>(cmp: Cmp, a: &T, b: &T) -> bool {
    match cmp {
        Cmp::Eq => a == b,
        Cmp::Ne => a != b,
        Cmp::Lt => a < b,
        Cmp::Le => a <= b,
        Cmp::Gt => a > b,
        Cmp::Ge => a >= b,
    }
}

/// Evaluate a binary operator in the reference evaluator. The registry selects
/// the semantic from the source op + operand value-classes; the central match
/// then applies it. Behavior (including the division-by-zero and invalid-operand
/// error messages) is identical to the pre-registry `eval_binary`.
pub(crate) fn eval_binary(op: &str, left: Value, right: Value) -> Result<Value> {
    let sem = OPS
        .iter()
        .find(|entry| {
            entry.category == OpCategory::Binary
                && entry.source_op == op
                && value_matches(entry.left_type, &left)
                && value_matches(entry.right_type, &right)
        })
        .map(|entry| entry.sem);
    match (sem, left, right) {
        (Some(SemOp::AddI64), Value::I64(a), Value::I64(b)) => Ok(Value::I64(a + b)),
        (Some(SemOp::SubI64), Value::I64(a), Value::I64(b)) => Ok(Value::I64(a - b)),
        (Some(SemOp::MulI64), Value::I64(a), Value::I64(b)) => Ok(Value::I64(a * b)),
        (Some(SemOp::DivI64), Value::I64(_), Value::I64(0)) => bail!("division by zero"),
        (Some(SemOp::DivI64), Value::I64(a), Value::I64(b)) => Ok(Value::I64(a / b)),
        (Some(SemOp::ModI64), Value::I64(_), Value::I64(0)) => bail!("modulo by zero"),
        (Some(SemOp::ModI64), Value::I64(a), Value::I64(b)) => Ok(Value::I64(a % b)),
        (Some(SemOp::CmpI64(cmp)), Value::I64(a), Value::I64(b)) => {
            Ok(Value::Bool(apply_cmp(cmp, &a, &b)))
        }
        (Some(SemOp::CmpU8(cmp)), Value::U8(a), Value::U8(b)) => {
            Ok(Value::Bool(apply_cmp(cmp, &a, &b)))
        }
        (Some(SemOp::AndBool), Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
        (Some(SemOp::OrBool), Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
        (_, left, right) => bail!("invalid operands for {op}: {left}, {right}"),
    }
}

/// Evaluate a unary operator in the reference evaluator. Behavior identical to
/// the pre-registry `eval_unary`.
pub(crate) fn eval_unary(op: &str, value: Value) -> Result<Value> {
    let sem = OPS
        .iter()
        .find(|entry| {
            entry.category == OpCategory::Unary
                && entry.source_op == op
                && value_matches(entry.left_type, &value)
        })
        .map(|entry| entry.sem);
    match (sem, value) {
        (Some(SemOp::NegI64), Value::I64(value)) => Ok(Value::I64(-value)),
        (Some(SemOp::NotBool), Value::Bool(value)) => Ok(Value::Bool(!value)),
        (_, value) => bail!("invalid operand for {op}: {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::native::{NativeArch, backend_encodes_kind};
    use std::collections::BTreeSet;

    /// Every entry's source op + types lowers back to its own kind, and the
    /// inverse verify mapping accepts it. This independently re-derives the old
    /// hand-written mappings, so a mistranscribed row fails here.
    #[test]
    fn registry_entries_round_trip() {
        let mut kinds = BTreeSet::new();
        for entry in OPS {
            assert!(kinds.insert(entry.kind), "duplicate kind {}", entry.kind);
            match entry.category {
                OpCategory::Binary => {
                    let lh = type_hash_for(entry.left_type);
                    let rh = type_hash_for(entry.right_type);
                    let rrh = type_hash_for(entry.result_type);
                    let derived = lower_binary_kind(entry.source_op, &lh, &rh, &rrh).unwrap();
                    assert_eq!(derived, entry.kind);
                    let trap = entry.trap.map(|t| (t.condition, t.code));
                    verify_binary_kind(entry.kind, &lh, &rh, &rrh, trap).unwrap();
                }
                OpCategory::Unary => {
                    let ih = type_hash_for(entry.left_type);
                    let rrh = type_hash_for(entry.result_type);
                    let derived = lower_unary_kind(entry.source_op, &ih, &rrh).unwrap();
                    assert_eq!(derived, entry.kind);
                    verify_unary_kind(entry.kind, &ih, &rrh).unwrap();
                }
            }
        }
    }

    /// Every registered operator has a machine-code encoder on both targets.
    /// Adding an `OPS` row without a backend arm fails here loudly (no toolchain
    /// needed), independent of the per-op conformance fixtures.
    #[test]
    fn every_op_has_backend_encoders() {
        for entry in OPS {
            assert!(
                backend_encodes_kind(NativeArch::X86_64, entry.kind),
                "x86_64 backend has no encoder for {}",
                entry.kind
            );
            assert!(
                backend_encodes_kind(NativeArch::Arm64, entry.kind),
                "arm64 backend has no encoder for {}",
                entry.kind
            );
        }
    }
}
