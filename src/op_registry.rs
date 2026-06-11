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
//! of that knowledge in one [`ops`] table; the former sites become thin
//! forwarders into the functions here. The only operator knowledge that stays
//! outside is the backend machine-code encoding (raw bytes per kind, which cannot
//! be table-generated) — and even that is guarded by a registry-driven coverage
//! test (see the tests at the bottom and `backend::native::backend_encodes_kind`).
//!
//! Sized integers (R5/R4/R6, Phase 9): the arithmetic, bitwise, shift, comparison,
//! and unary operators are generated for every width in
//! [`SCALAR_INT_TYPES`](crate::types::SCALAR_INT_TYPES). A [`SemOp`] carries the
//! [`IntKind`] (width + signedness) so the evaluator and the native backend
//! dispatch on a parametric description rather than a per-width string match.
//! Integer arithmetic is two's-complement **wrapping**; division/modulo by zero
//! **trap**. The existing `i64`/`u8` kind strings, traps, and evaluator results
//! are byte-identical to the pre-Phase-9 code.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};

use crate::expr::Value;
use crate::types::{SCALAR_INT_TYPES, type_hash_for};

/// Whether an entry is a binary or unary operator.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpCategory {
    Binary,
    Unary,
}

/// A comparison flavor; shared by every integer width's comparison semantics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The width (in bytes) and signedness of an integer operand/result. Threaded
/// through [`SemOp`] so the evaluator and the native backend can act on a width
/// uniformly (wrap to width, sign- vs zero-extend, signed vs unsigned divide/
/// compare) instead of matching every per-width kind string.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct IntKind {
    pub(crate) width: u64,
    pub(crate) signed: bool,
}

/// Wrapping integer arithmetic operators.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// Bitwise binary operators.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitOp {
    And,
    Or,
    Xor,
}

/// Shift operators (`Shr` is arithmetic for signed widths, logical for unsigned).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShiftOp {
    Shl,
    Shr,
}

/// The pure runtime semantic the reference evaluator interprets and the native
/// backend encodes. Integer variants carry their [`IntKind`]; the boolean
/// operators are width-free.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemOp {
    Arith(ArithOp, IntKind),
    Cmp(Cmp, IntKind),
    Bit(BitOp, IntKind),
    Shift(ShiftOp, IntKind),
    Neg(IntKind),
    BitNot(IntKind),
    AndBool,
    OrBool,
    NotBool,
}

/// A lowered-IR trap condition attached to a kind (division/modulo by zero).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrapSpec {
    pub(crate) condition: &'static str,
    pub(crate) code: &'static str,
}

/// One operator. `left_type`/`right_type`/`result_type` hold type *names*
/// (`"I64"`, `"U8"`, `"Bool"`); they are resolved to content hashes via
/// [`type_hash_for`] at comparison time so the registry can never drift from the
/// type system's hashing. For a unary operator, the operand type is stored in
/// `left_type` and `right_type` is `""`. `kind` is a `&'static str`: the bool
/// rows use literals, the generated integer rows leak a process-lifetime string.
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

/// Parser precedence for prefix unary operators (`-`, `!`, `~`).
pub(crate) const UNARY_PRECEDENCE: u8 = 70;

/// Precedence returned for any operator not in the binary table.
const DEFAULT_PRECEDENCE: u8 = 90;

// Binary precedence levels, low (binds loosest) to high. Scaled with gaps so
// new operators slot in without renumbering the relative order of the original
// operators (parse trees and projection parenthesization are invariant under a
// relative-order-preserving rescale, so no hash churn). Bitwise-OR is the
// LOWEST binary precedence so a case-arm body — parsed just above it — terminates
// at a top-level `|` (the arm separator) while still admitting every other
// operator; a bitwise `|` inside an arm body is written parenthesized.
const PREC_BIT_OR: u8 = 1;
const PREC_LOGICAL_OR: u8 = 10;
const PREC_LOGICAL_AND: u8 = 20;
const PREC_BIT_XOR: u8 = 24;
const PREC_BIT_AND: u8 = 26;
const PREC_EQUALITY: u8 = 30;
const PREC_RELATIONAL: u8 = 40;
const PREC_SHIFT: u8 = 45;
const PREC_ADDITIVE: u8 = 50;
const PREC_MULTIPLICATIVE: u8 = 60;

/// Minimum precedence a `case`-arm body is parsed at: one above bitwise-OR, so a
/// top-level `|` ends the arm instead of being consumed as an operator.
pub(crate) const CASE_ARM_BODY_MIN_PRECEDENCE: u8 = PREC_BIT_OR + 1;

const DIV_TRAP: TrapSpec = TrapSpec {
    condition: "right_operand_zero",
    code: "division_by_zero",
};

const MOD_TRAP: TrapSpec = TrapSpec {
    condition: "right_operand_zero",
    code: "modulo_by_zero",
};

/// THE operator table, built once. The three boolean operators are fixed rows;
/// the integer operators are generated for every width in `SCALAR_INT_TYPES`
/// (arithmetic, bitwise, shift, comparison, unary negate/complement). Generated
/// kind strings are `"{verb}_{lower-type}"` (e.g. `add_i64`, `xor_u32`,
/// `bitnot_u8`) — for `i64`/`u8` these reproduce the original kinds exactly.
pub(crate) fn ops() -> &'static [OpEntry] {
    static OPS: OnceLock<Vec<OpEntry>> = OnceLock::new();
    OPS.get_or_init(build_ops).as_slice()
}

fn leak_kind(verb: &str, type_name: &str) -> &'static str {
    Box::leak(format!("{verb}_{}", type_name.to_ascii_lowercase()).into_boxed_str())
}

fn build_ops() -> Vec<OpEntry> {
    let mut ops = Vec::new();
    // Boolean operators (width-free; kinds preserved verbatim).
    ops.push(bin(
        "and_bool", "&&", "Bool", "Bool", "Bool", None, PREC_LOGICAL_AND, SemOp::AndBool,
    ));
    ops.push(bin(
        "or_bool", "||", "Bool", "Bool", "Bool", None, PREC_LOGICAL_OR, SemOp::OrBool,
    ));
    ops.push(un("not_bool", "!", "Bool", "Bool", SemOp::NotBool));

    for int in SCALAR_INT_TYPES {
        let ty = int.name;
        let k = IntKind { width: int.width, signed: int.signed };

        // Arithmetic -> same width.
        for (verb, src, prec, trap, arith) in [
            ("add", "+", PREC_ADDITIVE, None, ArithOp::Add),
            ("sub", "-", PREC_ADDITIVE, None, ArithOp::Sub),
            ("mul", "*", PREC_MULTIPLICATIVE, None, ArithOp::Mul),
            ("div", "/", PREC_MULTIPLICATIVE, Some(DIV_TRAP), ArithOp::Div),
            ("mod", "%", PREC_MULTIPLICATIVE, Some(MOD_TRAP), ArithOp::Rem),
        ] {
            ops.push(bin(
                leak_kind(verb, ty), src, ty, ty, ty, trap, prec, SemOp::Arith(arith, k),
            ));
        }

        // Bitwise -> same width.
        for (verb, src, prec, bit) in [
            ("and", "&", PREC_BIT_AND, BitOp::And),
            ("or", "|", PREC_BIT_OR, BitOp::Or),
            ("xor", "^", PREC_BIT_XOR, BitOp::Xor),
        ] {
            ops.push(bin(
                leak_kind(verb, ty), src, ty, ty, ty, None, prec, SemOp::Bit(bit, k),
            ));
        }

        // Shifts -> left (== right) width.
        for (verb, src, shift) in
            [("shl", "<<", ShiftOp::Shl), ("shr", ">>", ShiftOp::Shr)]
        {
            ops.push(bin(
                leak_kind(verb, ty), src, ty, ty, ty, None, PREC_SHIFT, SemOp::Shift(shift, k),
            ));
        }

        // Comparisons -> Bool.
        for (verb, src, prec, cmp) in [
            ("eq", "==", PREC_EQUALITY, Cmp::Eq),
            ("ne", "!=", PREC_EQUALITY, Cmp::Ne),
            ("lt", "<", PREC_RELATIONAL, Cmp::Lt),
            ("le", "<=", PREC_RELATIONAL, Cmp::Le),
            ("gt", ">", PREC_RELATIONAL, Cmp::Gt),
            ("ge", ">=", PREC_RELATIONAL, Cmp::Ge),
        ] {
            ops.push(bin(
                leak_kind(verb, ty), src, ty, ty, "Bool", None, prec, SemOp::Cmp(cmp, k),
            ));
        }

        // Unary negate and bitwise complement -> same width.
        ops.push(un(leak_kind("neg", ty), "-", ty, ty, SemOp::Neg(k)));
        ops.push(un(leak_kind("bitnot", ty), "~", ty, ty, SemOp::BitNot(k)));
    }

    ops
}

/// All registered operator kinds, sorted. Public so the evaluator-vs-backend
/// conformance harness (`tests/oracle_conformance.rs`) can assert it carries a
/// fixture for *every* operator: adding an operator with no conformance fixture
/// then fails that coverage gate. This is the honesty link that keeps the oracle
/// complete as operators are added.
pub fn operator_kinds() -> Vec<&'static str> {
    let mut kinds: Vec<&'static str> = ops().iter().map(|entry| entry.kind).collect();
    kinds.sort_unstable();
    kinds
}

#[allow(clippy::too_many_arguments)]
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
        for entry in ops() {
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
    ops()
        .iter()
        .find(|entry| entry.category == OpCategory::Binary && entry.kind == kind)
}

fn lookup_unary_kind(kind: &str) -> Option<&'static OpEntry> {
    ops()
        .iter()
        .find(|entry| entry.category == OpCategory::Unary && entry.kind == kind)
}

/// The [`SemOp`] for a lowered kind, or `None` if `kind` is not a registered
/// operator. The native backend dispatches on this rather than the kind string.
pub(crate) fn sem_for_kind(kind: &str) -> Option<SemOp> {
    ops().iter().find(|entry| entry.kind == kind).map(|entry| entry.sem)
}

/// Whether `op` is a source-level binary operator (i.e. the registry carries a
/// binary entry for it). The parser routes its `is_binary_op` gate here so adding
/// an operator is a generated table row, not a second hand-maintained list.
pub(crate) fn is_source_binary_op(op: &str) -> bool {
    ops()
        .iter()
        .any(|entry| entry.category == OpCategory::Binary && entry.source_op == op)
}

/// Parser precedence for a binary operator (`DEFAULT_PRECEDENCE` for anything not
/// in the binary table). Filtering by category means `-` resolves to its binary
/// precedence, never the unary entry. All widths of an operator share one
/// precedence, so the first matching row suffices.
pub(crate) fn binary_precedence(op: &str) -> u8 {
    ops()
        .iter()
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
    ops()
        .iter()
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
    ops()
        .iter()
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
    ops().iter().find(|entry| entry.kind == kind).and_then(|e| e.trap)
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
        ("I8", Value::I8(_))
            | ("I16", Value::I16(_))
            | ("I32", Value::I32(_))
            | ("I64", Value::I64(_))
            | ("U8", Value::U8(_))
            | ("U16", Value::U16(_))
            | ("U32", Value::U32(_))
            | ("U64", Value::U64(_))
            | ("Bool", Value::Bool(_))
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
/// the semantic from the source op + operand value-classes; the central dispatch
/// then applies it with two's-complement wrapping at the operand width. The
/// division-by-zero/modulo-by-zero and invalid-operand error messages are
/// identical to the pre-registry `eval_binary`.
pub(crate) fn eval_binary(op: &str, left: Value, right: Value) -> Result<Value> {
    let sem = ops()
        .iter()
        .find(|entry| {
            entry.category == OpCategory::Binary
                && entry.source_op == op
                && value_matches(entry.left_type, &left)
                && value_matches(entry.right_type, &right)
        })
        .map(|entry| entry.sem);
    match sem {
        Some(SemOp::Arith(arith, _)) => eval_arith(arith, left, right),
        Some(SemOp::Cmp(cmp, _)) => eval_cmp(cmp, left, right),
        Some(SemOp::Bit(bit, _)) => eval_bit(bit, left, right),
        Some(SemOp::Shift(shift, _)) => eval_shift(shift, left, right),
        Some(SemOp::AndBool) => match (left, right) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
            (left, right) => bail!("invalid operands for {op}: {left}, {right}"),
        },
        Some(SemOp::OrBool) => match (left, right) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
            (left, right) => bail!("invalid operands for {op}: {left}, {right}"),
        },
        _ => bail!("invalid operands for {op}: {left}, {right}"),
    }
}

fn eval_arith(arith: ArithOp, left: Value, right: Value) -> Result<Value> {
    macro_rules! arm {
        ($x:ident, $y:ident, $ctor:path) => {{
            let v = match arith {
                ArithOp::Add => $x.wrapping_add($y),
                ArithOp::Sub => $x.wrapping_sub($y),
                ArithOp::Mul => $x.wrapping_mul($y),
                ArithOp::Div => {
                    if $y == 0 {
                        bail!("division by zero");
                    }
                    $x.wrapping_div($y)
                }
                ArithOp::Rem => {
                    if $y == 0 {
                        bail!("modulo by zero");
                    }
                    $x.wrapping_rem($y)
                }
            };
            Ok($ctor(v))
        }};
    }
    match (left, right) {
        (Value::I8(x), Value::I8(y)) => arm!(x, y, Value::I8),
        (Value::I16(x), Value::I16(y)) => arm!(x, y, Value::I16),
        (Value::I32(x), Value::I32(y)) => arm!(x, y, Value::I32),
        (Value::I64(x), Value::I64(y)) => arm!(x, y, Value::I64),
        (Value::U8(x), Value::U8(y)) => arm!(x, y, Value::U8),
        (Value::U16(x), Value::U16(y)) => arm!(x, y, Value::U16),
        (Value::U32(x), Value::U32(y)) => arm!(x, y, Value::U32),
        (Value::U64(x), Value::U64(y)) => arm!(x, y, Value::U64),
        (left, right) => bail!("invalid operands: {left}, {right}"),
    }
}

fn eval_bit(bit: BitOp, left: Value, right: Value) -> Result<Value> {
    macro_rules! arm {
        ($x:ident, $y:ident, $ctor:path) => {{
            let v = match bit {
                BitOp::And => $x & $y,
                BitOp::Or => $x | $y,
                BitOp::Xor => $x ^ $y,
            };
            Ok($ctor(v))
        }};
    }
    match (left, right) {
        (Value::I8(x), Value::I8(y)) => arm!(x, y, Value::I8),
        (Value::I16(x), Value::I16(y)) => arm!(x, y, Value::I16),
        (Value::I32(x), Value::I32(y)) => arm!(x, y, Value::I32),
        (Value::I64(x), Value::I64(y)) => arm!(x, y, Value::I64),
        (Value::U8(x), Value::U8(y)) => arm!(x, y, Value::U8),
        (Value::U16(x), Value::U16(y)) => arm!(x, y, Value::U16),
        (Value::U32(x), Value::U32(y)) => arm!(x, y, Value::U32),
        (Value::U64(x), Value::U64(y)) => arm!(x, y, Value::U64),
        (left, right) => bail!("invalid operands: {left}, {right}"),
    }
}

fn eval_shift(shift: ShiftOp, left: Value, right: Value) -> Result<Value> {
    // The shift amount is masked to the operand width by `wrapping_sh{l,r}`
    // (`amount % bits`); converting it through `as u32` preserves the low bits
    // the mask looks at, so this matches the backend (which masks the amount to
    // `width*8 - 1` before shifting). `wrapping_shr` is arithmetic for signed
    // widths and logical for unsigned, matching `asr`/`lsr`.
    macro_rules! arm {
        ($x:ident, $y:ident, $ctor:path) => {{
            let v = match shift {
                ShiftOp::Shl => $x.wrapping_shl($y as u32),
                ShiftOp::Shr => $x.wrapping_shr($y as u32),
            };
            Ok($ctor(v))
        }};
    }
    match (left, right) {
        (Value::I8(x), Value::I8(y)) => arm!(x, y, Value::I8),
        (Value::I16(x), Value::I16(y)) => arm!(x, y, Value::I16),
        (Value::I32(x), Value::I32(y)) => arm!(x, y, Value::I32),
        (Value::I64(x), Value::I64(y)) => arm!(x, y, Value::I64),
        (Value::U8(x), Value::U8(y)) => arm!(x, y, Value::U8),
        (Value::U16(x), Value::U16(y)) => arm!(x, y, Value::U16),
        (Value::U32(x), Value::U32(y)) => arm!(x, y, Value::U32),
        (Value::U64(x), Value::U64(y)) => arm!(x, y, Value::U64),
        (left, right) => bail!("invalid operands: {left}, {right}"),
    }
}

fn eval_cmp(cmp: Cmp, left: Value, right: Value) -> Result<Value> {
    macro_rules! arm {
        ($x:ident, $y:ident) => {
            Ok(Value::Bool(apply_cmp(cmp, &$x, &$y)))
        };
    }
    match (left, right) {
        (Value::I8(x), Value::I8(y)) => arm!(x, y),
        (Value::I16(x), Value::I16(y)) => arm!(x, y),
        (Value::I32(x), Value::I32(y)) => arm!(x, y),
        (Value::I64(x), Value::I64(y)) => arm!(x, y),
        (Value::U8(x), Value::U8(y)) => arm!(x, y),
        (Value::U16(x), Value::U16(y)) => arm!(x, y),
        (Value::U32(x), Value::U32(y)) => arm!(x, y),
        (Value::U64(x), Value::U64(y)) => arm!(x, y),
        (left, right) => bail!("invalid operands: {left}, {right}"),
    }
}

/// Evaluate a unary operator in the reference evaluator. Behavior identical to
/// the pre-registry `eval_unary` for `-`/`!`; `~` is bitwise complement.
pub(crate) fn eval_unary(op: &str, value: Value) -> Result<Value> {
    let sem = ops()
        .iter()
        .find(|entry| {
            entry.category == OpCategory::Unary
                && entry.source_op == op
                && value_matches(entry.left_type, &value)
        })
        .map(|entry| entry.sem);
    match sem {
        Some(SemOp::Neg(_)) => eval_neg(value),
        Some(SemOp::BitNot(_)) => eval_bitnot(value),
        Some(SemOp::NotBool) => match value {
            Value::Bool(v) => Ok(Value::Bool(!v)),
            value => bail!("invalid operand for {op}: {value}"),
        },
        _ => bail!("invalid operand for {op}: {value}"),
    }
}

fn eval_neg(value: Value) -> Result<Value> {
    match value {
        Value::I8(x) => Ok(Value::I8(x.wrapping_neg())),
        Value::I16(x) => Ok(Value::I16(x.wrapping_neg())),
        Value::I32(x) => Ok(Value::I32(x.wrapping_neg())),
        Value::I64(x) => Ok(Value::I64(x.wrapping_neg())),
        Value::U8(x) => Ok(Value::U8(x.wrapping_neg())),
        Value::U16(x) => Ok(Value::U16(x.wrapping_neg())),
        Value::U32(x) => Ok(Value::U32(x.wrapping_neg())),
        Value::U64(x) => Ok(Value::U64(x.wrapping_neg())),
        value => bail!("invalid operand for -: {value}"),
    }
}

fn eval_bitnot(value: Value) -> Result<Value> {
    match value {
        Value::I8(x) => Ok(Value::I8(!x)),
        Value::I16(x) => Ok(Value::I16(!x)),
        Value::I32(x) => Ok(Value::I32(!x)),
        Value::I64(x) => Ok(Value::I64(!x)),
        Value::U8(x) => Ok(Value::U8(!x)),
        Value::U16(x) => Ok(Value::U16(!x)),
        Value::U32(x) => Ok(Value::U32(!x)),
        Value::U64(x) => Ok(Value::U64(!x)),
        value => bail!("invalid operand for ~: {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::native::{NativeArch, backend_encodes_kind};
    use std::collections::BTreeSet;

    /// Every entry's source op + types lowers back to its own kind, and the
    /// inverse verify mapping accepts it. This independently re-derives the
    /// generated kind mappings, so a mistranscribed row fails here.
    #[test]
    fn registry_entries_round_trip() {
        let mut kinds = BTreeSet::new();
        for entry in ops() {
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

    /// The original i64/u8/bool kinds are still present with their exact strings
    /// and traps — generating the table must not have perturbed the wire format.
    #[test]
    fn preserves_original_kinds() {
        for kind in [
            "add_i64", "sub_i64", "mul_i64", "div_i64", "mod_i64", "eq_i64", "ne_i64", "lt_i64",
            "le_i64", "gt_i64", "ge_i64", "eq_u8", "ne_u8", "lt_u8", "le_u8", "gt_u8", "ge_u8",
            "and_bool", "or_bool", "neg_i64", "not_bool",
        ] {
            assert!(sem_for_kind(kind).is_some(), "missing original kind {kind}");
        }
        assert_eq!(binary_trap("div_i64").unwrap().code, "division_by_zero");
        assert_eq!(binary_trap("mod_i64").unwrap().code, "modulo_by_zero");
    }

    /// Every registered operator has a machine-code encoder on both targets.
    /// Adding a row without a backend arm fails here loudly (no toolchain
    /// needed), independent of the per-op conformance fixtures.
    #[test]
    fn every_op_has_backend_encoders() {
        for entry in ops() {
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
