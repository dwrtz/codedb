use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display};
use std::rc::Rc;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::MAIN_BRANCH;
use crate::model::{
    NameBinding, ProgramRootPayload, RootSymbolPayload, param_names, preferred_names,
    preferred_type_names, root_module_names,
};
use crate::store::{CodeDb, canonical_json};
use crate::types::{
    Effect, ParamSpec, RegionParamDef, SymbolBirthSpec, TypeDefinition, TypeDefinitionIdentity,
    TypeDefinitionKind, TypeMemberSpec, TypeSpec, bytes_to_hex, hex_to_bytes, normalize_effects,
    static_data_payload, visible_effects,
};

/// What a [`RawConversionHook`] tells the typed→raw converter to do at one
/// node. Patch reconstruction (#12) hooks the ONE complete converter instead
/// of maintaining parallel partial converters per patch flavor — those drifted
/// (loop/return/case/record kinds missing) and broke patching for programs
/// using newer expression kinds.
pub(crate) enum RawHookOutcome {
    /// Use this raw expression instead of converting the node (and do not
    /// descend into it).
    Replace(RawExpr),
    /// The node must be a call: convert its arguments normally (the hook stays
    /// active inside them), but rename the callee.
    RenameCall(String),
    /// The node must be a call: inline the callee's body at the call site
    /// (patch `inline_function`).
    InlineCall,
    /// Convert the node normally.
    Continue,
}

/// Node-level hook for [`CodeDb::typed_expr_to_raw_hooked`], consulted at
/// every node before ordinary conversion. Keyed by content hash, so every
/// occurrence of an identical subtree replaces alike (content addressing has
/// no positional identity).
pub(crate) type RawConversionHook<'a> = &'a dyn Fn(&str, &JsonValue) -> Result<RawHookOutcome>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RawExpr {
    LiteralI64 {
        value: String,
    },
    LiteralBool {
        value: bool,
    },
    LiteralString {
        value: String,
    },
    LiteralBytes {
        bytes_hex: String,
    },
    Unit,
    ParamRef {
        index: usize,
    },
    ParamName {
        name: String,
    },
    Call {
        name: String,
        args: Vec<RawExpr>,
    },
    Binary {
        op: String,
        left: Box<RawExpr>,
        right: Box<RawExpr>,
    },
    Unary {
        op: String,
        expr: Box<RawExpr>,
    },
    BorrowShared {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        target: Box<RawExpr>,
    },
    BorrowMut {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        target: Box<RawExpr>,
    },
    Assign {
        target: Box<RawExpr>,
        value: Box<RawExpr>,
    },
    Let {
        name: String,
        #[serde(rename = "type")]
        ty: String,
        value: Box<RawExpr>,
        body: Box<RawExpr>,
    },
    If {
        cond: Box<RawExpr>,
        #[serde(rename = "then")]
        then_expr: Box<RawExpr>,
        #[serde(rename = "else")]
        else_expr: Box<RawExpr>,
    },
    /// `return <expr>` — early exit (R7). Evaluating it abandons the rest of the
    /// enclosing function and yields `<expr>` as the function's result. It is a
    /// *divergent* expression: it produces no value to its own context, so it is
    /// only well-formed in a "block-result" position (a function body, an `if`
    /// then/else branch, a `case` arm body, or a `let` body) — see
    /// `raw_expr_diverges` and the position check in type-checking. The early-exit
    /// edge drops every owned value still live at the return point (SPEC_V3 §7).
    Return {
        value: Box<RawExpr>,
    },
    Fold {
        item: String,
        target: Box<RawExpr>,
        acc: String,
        init: Box<RawExpr>,
        body: Box<RawExpr>,
    },
    /// `loop <acc> = <init> while <cond> do <body>` — a condition-driven loop (R8).
    /// Carries one loop accumulator `acc` (initialized to `init`); each iteration,
    /// if `cond` (a `bool` over `acc`) holds, `acc` becomes `body` (the next
    /// accumulator, same type), else the loop exits yielding the final `acc`. Both
    /// `cond` and `body` see `acc` in scope. The condition-driven counterpart of
    /// `Fold`: like `fold`, the accumulator must be copyable and the body may not
    /// move owned values (loop-carried drop glue is a follow-on; SPEC_V3 §7).
    Loop {
        acc: String,
        init: Box<RawExpr>,
        cond: Box<RawExpr>,
        body: Box<RawExpr>,
    },
    Array {
        elements: Vec<RawExpr>,
    },
    /// `[<value>; <count>]` — array repeat/fill initializer (R9). `value` is
    /// evaluated ONCE and replicated into all `count` slots, yielding an
    /// `array<T, count>`. `value`'s type must be Copy (it is replicated). `count`
    /// is a non-negative integer literal (the array size is a compile-time
    /// constant), stored as its decimal digits.
    ArrayFill {
        value: Box<RawExpr>,
        count: String,
    },
    Index {
        target: Box<RawExpr>,
        index: Box<RawExpr>,
    },
    Record {
        fields: Vec<RawRecordField>,
    },
    FieldAccess {
        target: Box<RawExpr>,
        field: String,
    },
    EnumConstruct {
        #[serde(rename = "type")]
        enum_type: String,
        variant: String,
        value: Box<RawExpr>,
    },
    Case {
        expr: Box<RawExpr>,
        arms: Vec<RawCaseArm>,
    },
}

/// Whether evaluating `expr` always exits the enclosing function early (R7) — it
/// is a `return`, or a conditional whose every continuation path is itself
/// divergent. A divergent expression yields no value to its own context, so the
/// `if`/`case` type-join treats a divergent branch as having "no type" (the
/// other, non-divergent branch fixes the result type) and lowering routes the
/// branch to the early-exit edge rather than the merge. Structural and pure: it
/// reads only the shape, never types.
pub(crate) fn raw_expr_diverges(expr: &RawExpr) -> bool {
    match expr {
        RawExpr::Return { .. } => true,
        RawExpr::If {
            then_expr,
            else_expr,
            ..
        } => raw_expr_diverges(then_expr) && raw_expr_diverges(else_expr),
        RawExpr::Case { arms, .. } => {
            !arms.is_empty() && arms.iter().all(|arm| raw_expr_diverges(&arm.body))
        }
        RawExpr::Let { body, .. } => raw_expr_diverges(body),
        _ => false,
    }
}

/// Reject an early `return` (R7) that is not in a "block-result" position. A
/// `return` yields no value to its own context, so it is only well-formed where
/// its value *is* the enclosing computation's result on that path: the function
/// body, an `if` then/else branch, a `case` arm body, or a `let` body. Every
/// other position (a condition, scrutinee, `let` value, operand, argument, index,
/// field, or `fold` sub-expression) evaluates its child for a value, so a
/// `return` there is rejected — keeping lowering's early-exit edge confined to
/// block boundaries. Branches/bodies are return-allowed regardless of the
/// enclosing context, so `let b = (if c then return e else next) in rest` is
/// well-formed (the `return` is an `if` branch) while `let x = return e in body`
/// and `f(return e)` are not. Exhaustive over `RawExpr`, so a new form must state
/// its return-position policy.
pub(crate) fn validate_return_positions(expr: &RawExpr, allowed: bool) -> Result<()> {
    match expr {
        RawExpr::Return { value } => {
            if !allowed {
                bail!(
                    "`return` may only appear as a function body, an `if`/`case` branch, or a \
                     `let` body — not in a condition, scrutinee, `let` value, operand, argument, \
                     index, field, or `fold` body"
                );
            }
            // A return's operand is a value position, not a block result.
            validate_return_positions(value, false)
        }
        RawExpr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            validate_return_positions(cond, false)?;
            validate_return_positions(then_expr, true)?;
            validate_return_positions(else_expr, true)
        }
        RawExpr::Let { value, body, .. } => {
            validate_return_positions(value, false)?;
            validate_return_positions(body, true)
        }
        RawExpr::Case { expr, arms } => {
            validate_return_positions(expr, false)?;
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    validate_return_positions(guard, false)?;
                }
                validate_return_positions(&arm.body, true)?;
            }
            Ok(())
        }
        // A `fold` body's value is the accumulator, so a DIRECT `return` (the
        // always-diverges degenerate form — the fold would be pointless) is a
        // strict value position. An `if`/`case` branch inside it re-grants the
        // block-result position (the arms above pass `true` unconditionally),
        // so a CONDITIONAL `return` is allowed — it exits the whole function,
        // not just the fold, identically in eval and native (#14c, blessed and
        // pinned by tests/early_exit_native.rs). `break` (exit the loop with a
        // value, R8) remains deferred and is a different construct.
        RawExpr::Fold {
            target, init, body, ..
        } => {
            validate_return_positions(target, false)?;
            validate_return_positions(init, false)?;
            validate_return_positions(body, false)
        }
        // Same rule as `fold`: a DIRECT `return` as init/cond/body is the
        // degenerate always-diverges form and is rejected; a conditional
        // `return` under an `if`/`case` inside them exits the whole function
        // and is supported (#14c — the search-loop early exit).
        RawExpr::Loop {
            init, cond, body, ..
        } => {
            validate_return_positions(init, false)?;
            validate_return_positions(cond, false)?;
            validate_return_positions(body, false)
        }
        RawExpr::Call { args, .. } => {
            for arg in args {
                validate_return_positions(arg, false)?;
            }
            Ok(())
        }
        RawExpr::Binary { left, right, .. } => {
            validate_return_positions(left, false)?;
            validate_return_positions(right, false)
        }
        RawExpr::Unary { expr, .. } => validate_return_positions(expr, false),
        RawExpr::BorrowShared { target, .. } | RawExpr::BorrowMut { target, .. } => {
            validate_return_positions(target, false)
        }
        RawExpr::Assign { target, value } => {
            validate_return_positions(target, false)?;
            validate_return_positions(value, false)
        }
        RawExpr::Array { elements } => {
            for element in elements {
                validate_return_positions(element, false)?;
            }
            Ok(())
        }
        RawExpr::ArrayFill { value, .. } => validate_return_positions(value, false),
        RawExpr::Index { target, index } => {
            validate_return_positions(target, false)?;
            validate_return_positions(index, false)
        }
        RawExpr::Record { fields } => {
            for field in fields {
                validate_return_positions(&field.value, false)?;
            }
            Ok(())
        }
        RawExpr::FieldAccess { target, .. } => validate_return_positions(target, false),
        RawExpr::EnumConstruct { value, .. } => validate_return_positions(value, false),
        RawExpr::LiteralI64 { .. }
        | RawExpr::LiteralBool { .. }
        | RawExpr::LiteralString { .. }
        | RawExpr::LiteralBytes { .. }
        | RawExpr::Unit
        | RawExpr::ParamRef { .. }
        | RawExpr::ParamName { .. } => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRecordField {
    pub name: String,
    pub value: RawExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCaseArm {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// A scalar literal pattern (an `i64` or `bool` literal) for matching on a
    /// scalar scrutinee (R14). Mutually exclusive with `variant`/`default`/`range`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<Box<RawExpr>>,
    /// A scalar `i64` range pattern (`lo..hi` / `lo..=hi`, R14). Mutually exclusive
    /// with `variant`/`default`/`literal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<RawCaseRange>,
    #[serde(default, skip_serializing_if = "raw_case_arm_default_is_false")]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// An optional `if <expr>` guard (R14): the arm matches only when its pattern
    /// matches AND this `bool` predicate evaluates true; otherwise control falls
    /// through to the next arm. A guarded arm never proves exhaustiveness, and a
    /// guarded wildcard (`_ if g`) need not be last. Guards must be pure (no moves,
    /// no effects); currently only an `i64` scalar `case` arm may carry one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<Box<RawExpr>>,
    /// A nested enum-destructuring payload pattern (R14): the matched variant's
    /// payload is itself matched against an inner variant pattern (e.g.
    /// `some(inner(x))` / `some(inner(_))`), recursively. Present only for a
    /// nested arm; mutually exclusive with `binding` (a nested chain binds its one
    /// leaf name inside the pattern). Absent (skip-if-none) for every simple arm,
    /// so existing typed nodes are byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_pattern: Option<RawPattern>,
    pub body: RawExpr,
}

/// A nested enum-destructuring pattern (R14). A variant carries exactly one
/// payload, so a pattern *chain* binds at most one leaf name (the deepest
/// `Binding`); the intermediate `Variant` levels are pure variant tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RawPattern {
    /// `x` — bind the matched value to a fresh local (the chain's leaf).
    Binding(String),
    /// `_` — match anything, bind nothing (a leaf wildcard).
    Wildcard,
    /// `v(sub)` — match enum variant `v`, then match its payload against `sub`.
    Variant {
        variant: String,
        sub: Box<RawPattern>,
    },
}

/// An `i64` range case pattern (R14): `lo..hi` (exclusive) or `lo..=hi` (inclusive).
/// `lo`/`hi` are `i64` literals (a number, optionally negated), held as `RawExpr`
/// so the projection round-trips structurally (SPEC_V3 §11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCaseRange {
    pub lo: Box<RawExpr>,
    pub hi: Box<RawExpr>,
    #[serde(default, skip_serializing_if = "raw_case_arm_default_is_false")]
    pub inclusive: bool,
}

fn raw_case_arm_default_is_false(value: &bool) -> bool {
    !*value
}

/// Reconstruct a scalar literal case pattern (R14) from a typed case arm payload
/// (`{"literal_i64": "..."}` / `{"literal_bool": ...}`), or `None` if the arm is
/// not a scalar literal pattern (a variant or default arm).
pub(crate) fn scalar_literal_pattern_from_typed_arm(arm: &JsonValue) -> Option<Box<RawExpr>> {
    if let Some(value) = arm.get("literal_i64").and_then(JsonValue::as_str) {
        return Some(Box::new(RawExpr::LiteralI64 {
            value: value.to_string(),
        }));
    }
    if let Some(value) = arm.get("literal_bool").and_then(JsonValue::as_bool) {
        return Some(Box::new(RawExpr::LiteralBool { value }));
    }
    None
}

fn typed_case_arm_is_default(arm: &JsonValue) -> bool {
    arm.get("default").and_then(JsonValue::as_bool) == Some(true)
}

/// Reconstruct an `i64` range case pattern (R14) from a typed case arm payload
/// (`{"range_lo": "..", "range_hi": "..", "range_inclusive": bool}`), or `None` if
/// the arm is not a range pattern.
pub(crate) fn scalar_range_pattern_from_typed_arm(arm: &JsonValue) -> Option<RawCaseRange> {
    let lo = arm.get("range_lo").and_then(JsonValue::as_str)?;
    let hi = arm.get("range_hi").and_then(JsonValue::as_str)?;
    let inclusive = arm
        .get("range_inclusive")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    Some(RawCaseRange {
        lo: Box::new(RawExpr::LiteralI64 {
            value: lo.to_string(),
        }),
        hi: Box::new(RawExpr::LiteralI64 {
            value: hi.to_string(),
        }),
        inclusive,
    })
}

/// Does a typed scalar-`case` arm match an `i64` scrutinee? True for a `literal_i64`
/// equal to `scrutinee`, or a `range_lo..range_hi` pattern (inclusive when
/// `range_inclusive`) that contains it. Shared by the evaluator, tracer, and
/// debugger so first-match order is identical everywhere.
pub(crate) fn scalar_i64_arm_matches(arm: &JsonValue, scrutinee: i64) -> bool {
    if let Some(value) = arm
        .get("literal_i64")
        .and_then(JsonValue::as_str)
        .and_then(|value| value.parse::<i64>().ok())
    {
        return value == scrutinee;
    }
    if let (Some(lo), Some(hi)) = (
        arm.get("range_lo")
            .and_then(JsonValue::as_str)
            .and_then(|value| value.parse::<i64>().ok()),
        arm.get("range_hi")
            .and_then(JsonValue::as_str)
            .and_then(|value| value.parse::<i64>().ok()),
    ) {
        let inclusive = arm
            .get("range_inclusive")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        return scrutinee >= lo && if inclusive { scrutinee <= hi } else { scrutinee < hi };
    }
    false
}

/// Reconstruct a nested enum-destructuring pattern (R14) from a typed `case` arm,
/// or `None` when the arm carries no nested `payload_pattern`. A pattern node
/// mirrors the arm shape — `{"variant": v, "binding_name"?: x, "payload_pattern"?:
/// {..}}` — with a `binding_name` leaf, a deeper `payload_pattern` level, or
/// neither (a `_` wildcard leaf). Shared by the projector, `typed→raw`, and the
/// rename rewriter so the surface form round-trips identically everywhere.
pub(crate) fn nested_pattern_from_typed_arm(arm: &JsonValue) -> Option<RawPattern> {
    arm.get("payload_pattern").map(nested_pattern_from_node)
}

/// A typed enum `case` arm as a pattern over the *scrutinee* enum (R14), or `None`
/// for a `default` arm. Mirrors `arm_scrutinee_pattern` on the raw side; drives the
/// re-verify exhaustiveness check.
pub(crate) fn typed_arm_scrutinee_pattern(arm: &JsonValue) -> Option<RawPattern> {
    if typed_case_arm_is_default(arm) {
        return None;
    }
    let variant = arm.get("variant").and_then(JsonValue::as_str)?.to_string();
    let sub = if let Some(pattern) = nested_pattern_from_typed_arm(arm) {
        pattern
    } else if let Some(name) = arm.get("binding_name").and_then(JsonValue::as_str) {
        RawPattern::Binding(name.to_string())
    } else {
        RawPattern::Wildcard
    };
    Some(RawPattern::Variant {
        variant,
        sub: Box::new(sub),
    })
}

/// First-match an enum `case` arm/pattern node against a scrutinee value (R14).
/// `node` is `{"variant", "binding_name"?, "payload_pattern"?}`; returns `None` when
/// the value's variant chain doesn't match, or `Some(leaf)` when it does — `leaf`
/// being the value cell to bind for the body (`Some` for a `binding_name` leaf,
/// `None` for a wildcard/no-binding leaf). Recurses through nested patterns,
/// mirroring the decision-tree lowering so first-match order is identical.
/// Wrap an enum value (variant + payload cell) in a fresh value cell — the scrutinee
/// form `match_typed_pattern` matches against.
pub(crate) fn enum_cell(variant: String, value: ValueCell) -> ValueCell {
    Rc::new(RefCell::new(Value::Enum { variant, value }))
}

pub(crate) fn match_typed_pattern(node: &JsonValue, value: &ValueCell) -> Option<Option<ValueCell>> {
    let payload = {
        let borrowed = value.borrow();
        let Value::Enum { variant, value: payload } = &*borrowed else {
            return None;
        };
        if Some(variant.as_str()) != node.get("variant").and_then(JsonValue::as_str) {
            return None;
        }
        payload.clone()
    };
    if node.get("binding_name").and_then(JsonValue::as_str).is_some() {
        Some(Some(payload))
    } else if let Some(inner) = node.get("payload_pattern") {
        match_typed_pattern(inner, &payload)
    } else {
        Some(None)
    }
}

fn nested_pattern_from_node(node: &JsonValue) -> RawPattern {
    let variant = node
        .get("variant")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    let sub = if let Some(name) = node.get("binding_name").and_then(JsonValue::as_str) {
        RawPattern::Binding(name.to_string())
    } else if let Some(inner) = node.get("payload_pattern") {
        nested_pattern_from_node(inner)
    } else {
        RawPattern::Wildcard
    };
    RawPattern::Variant {
        variant,
        sub: Box::new(sub),
    }
}

/// The single leaf binding name of a nested pattern chain (`None` for a wildcard
/// leaf). A variant has one payload, so a chain binds at most one name — the one
/// the arm body sees in scope.
pub(crate) fn pattern_leaf_binding(pattern: &RawPattern) -> Option<&str> {
    match pattern {
        RawPattern::Binding(name) => Some(name),
        RawPattern::Wildcard => None,
        RawPattern::Variant { sub, .. } => pattern_leaf_binding(sub),
    }
}

/// Render a nested case pattern (R14) to its re-parseable surface form: `x`, `_`,
/// or `v(sub)` (recursively). The projector wraps this in the arm's outer variant.
pub(crate) fn render_pattern(pattern: &RawPattern) -> String {
    match pattern {
        RawPattern::Binding(name) => name.clone(),
        RawPattern::Wildcard => "_".to_string(),
        RawPattern::Variant { variant, sub } => format!("{variant}({})", render_pattern(sub)),
    }
}

/// The local binding name a typed enum `case` arm puts in scope for its body — the
/// simple `binding_name`, or (R14) a nested `payload_pattern` chain's leaf
/// `binding_name`. `None` when the arm binds nothing. Shared by every typed-arm
/// walker that scopes the arm body (re-verify, borrow/effect/loan/escape analysis,
/// the evaluator, the tracer) so nested bindings are seen identically everywhere.
pub(crate) fn typed_arm_binding_name(arm: &JsonValue) -> Option<&str> {
    if let Some(name) = arm.get("binding_name").and_then(JsonValue::as_str) {
        return Some(name);
    }
    let mut node = arm.get("payload_pattern")?;
    loop {
        if let Some(name) = node.get("binding_name").and_then(JsonValue::as_str) {
            return Some(name);
        }
        node = node.get("payload_pattern")?;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSource {
    pub module: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub region_params: Vec<String>,
    /// Type parameters on a generic function, e.g. the `T` in
    /// `fn id<T>(x: T) -> T` (R11). Empty for a non-generic function.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_params: Vec<String>,
    pub params: Vec<ParamSpec>,
    pub return_type: String,
    #[serde(default)]
    pub effects: Vec<Effect>,
    pub body: RawExpr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalFunctionSource {
    pub module: String,
    pub name: String,
    pub region_params: Vec<String>,
    pub params: Vec<ParamSpec>,
    pub return_type: String,
    pub effects: Vec<Effect>,
    pub abi: String,
    pub link_name: String,
    pub library: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramItem {
    TypeDefinition(TypeDefinitionSource),
    Function(FunctionSource),
    ExternalFunction(ExternalFunctionSource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDefinitionSource {
    pub module: String,
    pub name: String,
    pub region_params: Vec<String>,
    /// Type parameters on a generic record/enum, e.g. the `T` in
    /// `record Pair<T>` / `enum Option<T>` (R11). Empty for a non-generic type.
    pub type_params: Vec<String>,
    pub definition: TypeDefinitionKind,
    pub(crate) identity: Option<TypeDefinitionIdentity>,
}

#[derive(Debug, Clone)]
pub enum Value {
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    Bool(bool),
    Unit,
    SharedRef(ValueCell),
    MutRef(ValueCell),
    RawPtr {
        target: ValueCell,
        mutable: bool,
    },
    Boxed(ValueCell),
    Slice {
        elements: Vec<ValueCell>,
        mutable: bool,
    },
    Vec {
        elements: Vec<ValueCell>,
        capacity: usize,
    },
    String(Vec<u8>),
    Array(Vec<ValueCell>),
    Record(BTreeMap<String, ValueCell>),
    Enum {
        variant: String,
        value: ValueCell,
    },
}

pub type ValueCell = Rc<RefCell<Value>>;

thread_local! {
    /// Out-of-band slot carrying an early-`return` value (R7) while the sentinel
    /// error unwinds to the function-call boundary. A `Value` holds an `Rc`, which
    /// is not `Send + Sync`, so it cannot be carried inside an `anyhow::Error`;
    /// instead the value is stashed here and read back by `take_return_unwind` at
    /// the nearest `eval_symbol`/`trace_symbol`. Unwinding is immediate and
    /// single-threaded — the value is written, the sentinel propagates straight up
    /// through `?`, and the boundary takes it before any other `return` can run —
    /// so a single slot (per thread) suffices and need not be a stack.
    static RETURN_UNWIND_VALUE: RefCell<Option<Value>> = const { RefCell::new(None) };
}

/// The unwinding signal an early `return` raises; recognized by `eval_symbol` /
/// `trace_symbol` via `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub(crate) struct ReturnUnwind;

impl std::fmt::Display for ReturnUnwind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "early return propagated past the enclosing function boundary")
    }
}

impl std::error::Error for ReturnUnwind {}

/// Stash an early-`return` value and produce the sentinel error that unwinds to
/// the function-call boundary (R7).
pub(crate) fn raise_return_unwind(value: Value) -> anyhow::Error {
    RETURN_UNWIND_VALUE.with(|slot| *slot.borrow_mut() = Some(value));
    anyhow::Error::new(ReturnUnwind)
}

/// At a function-call boundary, recover the value a propagating early `return`
/// stashed. `Some(value)` converts the unwind into the call's result; `None`
/// means the slot was empty (a malformed unwind), surfaced as an error.
pub(crate) fn take_return_unwind(err: anyhow::Error) -> Result<Value> {
    if err.downcast_ref::<ReturnUnwind>().is_some() {
        RETURN_UNWIND_VALUE
            .with(|slot| slot.borrow_mut().take())
            .ok_or_else(|| anyhow!("early-return unwind slot was empty"))
    } else {
        Err(err)
    }
}

/// Parse an integer literal's text (decimal, or `0x`/`0X` hex) into the `Value` of
/// the given sized-integer type. The single place the evaluator widens a literal
/// token to a typed value; its radix handling mirrors `int_literal_in_range`, so a
/// literal that type-checks parses here.
/// The canonical 64-bit constant for an integer literal: the value (parsed at its
/// width, hex or decimal) reinterpreted as `i64`. Lowering stores this so the
/// backend and constant-index map read one decimal form regardless of width or
/// radix (a u64 with the high bit set becomes a negative i64 bit pattern).
pub(crate) fn int_literal_const_i64(value: &str, int: &crate::types::ScalarIntType) -> Result<i64> {
    Ok(match int_literal_value(value, int)? {
        Value::I8(x) => i64::from(x),
        Value::I16(x) => i64::from(x),
        Value::I32(x) => i64::from(x),
        Value::I64(x) => x,
        Value::U8(x) => i64::from(x),
        Value::U16(x) => i64::from(x),
        Value::U32(x) => i64::from(x),
        Value::U64(x) => x as i64,
        other => bail!("integer literal produced a non-integer value: {other}"),
    })
}

/// Cast an integer [`Value`] to a target width/signedness with the `as` semantics
/// the native backend reproduces (truncate on narrowing, sign-/zero-extend on
/// widening, bit-reinterpret on a same-width sign change). The source's
/// mathematical value goes through `i128`, then a final `as` to the target type
/// applies the wrap/reinterpret.
pub(crate) fn cast_int_value(value: &Value, target: &crate::types::ScalarIntType) -> Result<Value> {
    let x: i128 = match value {
        Value::I8(n) => i128::from(*n),
        Value::I16(n) => i128::from(*n),
        Value::I32(n) => i128::from(*n),
        Value::I64(n) => i128::from(*n),
        Value::U8(n) => i128::from(*n),
        Value::U16(n) => i128::from(*n),
        Value::U32(n) => i128::from(*n),
        Value::U64(n) => i128::from(*n),
        other => bail!("cannot cast non-integer value {other}"),
    };
    Ok(match (target.signed, target.width) {
        (true, 1) => Value::I8(x as i8),
        (true, 2) => Value::I16(x as i16),
        (true, 4) => Value::I32(x as i32),
        (true, 8) => Value::I64(x as i64),
        (false, 1) => Value::U8(x as u8),
        (false, 2) => Value::U16(x as u16),
        (false, 4) => Value::U32(x as u32),
        (false, 8) => Value::U64(x as u64),
        _ => bail!("unsupported cast target width {}", target.width),
    })
}

pub(crate) fn int_literal_value(value: &str, int: &crate::types::ScalarIntType) -> Result<Value> {
    let (radix, digits) = match value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        Some(hex) => (16, hex),
        None => (10, value),
    };
    let parse_err = || anyhow!("integer literal {value} out of range for {}", int.name);
    // Signed hex is a bit pattern (#9), mirroring `int_literal_in_range`:
    // parsed at the unsigned width, reinterpreted two's-complement.
    let signed_hex = int.signed && radix == 16;
    Ok(match (int.signed, int.width) {
        (true, 1) if signed_hex => {
            Value::I8(u8::from_str_radix(digits, radix).map_err(|_| parse_err())? as i8)
        }
        (true, 2) if signed_hex => {
            Value::I16(u16::from_str_radix(digits, radix).map_err(|_| parse_err())? as i16)
        }
        (true, 4) if signed_hex => {
            Value::I32(u32::from_str_radix(digits, radix).map_err(|_| parse_err())? as i32)
        }
        (true, 8) if signed_hex => {
            Value::I64(u64::from_str_radix(digits, radix).map_err(|_| parse_err())? as i64)
        }
        (true, 1) => Value::I8(i8::from_str_radix(digits, radix).map_err(|_| parse_err())?),
        (true, 2) => Value::I16(i16::from_str_radix(digits, radix).map_err(|_| parse_err())?),
        (true, 4) => Value::I32(i32::from_str_radix(digits, radix).map_err(|_| parse_err())?),
        (true, 8) => Value::I64(i64::from_str_radix(digits, radix).map_err(|_| parse_err())?),
        (false, 1) => Value::U8(u8::from_str_radix(digits, radix).map_err(|_| parse_err())?),
        (false, 2) => Value::U16(u16::from_str_radix(digits, radix).map_err(|_| parse_err())?),
        (false, 4) => Value::U32(u32::from_str_radix(digits, radix).map_err(|_| parse_err())?),
        (false, 8) => Value::U64(u64::from_str_radix(digits, radix).map_err(|_| parse_err())?),
        _ => bail!("unsupported integer width {}", int.width),
    })
}

/// Reference-evaluator call-recursion ceiling. The evaluator is a host-stack
/// tree-walker (`eval_symbol` -> `eval_expr` -> ... -> `eval_symbol`), so a deeply
/// or non-terminating recursive program would overflow the OS thread stack and
/// abort the whole process (SIGABRT) instead of returning an error. This ceiling
/// converts that crash into a clean, recoverable `Result::Err`.
///
/// It is an ORACLE robustness bound, not a language limit: the native backend runs
/// on the OS stack with no such ceiling, so a program that exceeds this still
/// compiles and runs natively — only the reference evaluator declines it. Sized
/// below the empirical overflow depth on the default main-thread stack (where the
/// CLI / oracle evaluates); debug stack frames are ~10x larger than release, hence
/// the split. This bounds call recursion (the real unbounded hazard); pure
/// expression nesting is bounded by source size.
const MAX_EVAL_CALL_DEPTH: usize = if cfg!(debug_assertions) { 120 } else { 1000 };

/// Per-`loop` iteration ceiling for the reference evaluator (R8). Like
/// [`MAX_EVAL_CALL_DEPTH`], this is an ORACLE robustness bound, not a language
/// limit: a condition-driven loop may not terminate, which would hang the
/// evaluator (an iterative loop does not grow the host stack, so it cannot abort
/// — it would spin forever). The native backend runs the same loop with no
/// ceiling. Sized far above any realistic fixpoint/worklist iteration count, so it
/// only ever fires on a genuinely non-terminating loop, converting a hang into a
/// clean error.
pub(crate) const MAX_EVAL_LOOP_ITERATIONS: u64 = 100_000_000;

thread_local! {
    static EVAL_CALL_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// RAII guard that bounds reference-evaluator call recursion (see
/// [`MAX_EVAL_CALL_DEPTH`]). Increments the per-thread depth on `enter` and
/// decrements on `Drop`, so the count is restored on every return path — including
/// `?` early returns and the bail below. On overflow it returns an error WITHOUT
/// incrementing, so a rejected entry leaves the counter untouched.
struct EvalCallDepthGuard;

impl EvalCallDepthGuard {
    fn enter() -> Result<Self> {
        EVAL_CALL_DEPTH.with(|depth| {
            let next = depth.get() + 1;
            if next > MAX_EVAL_CALL_DEPTH {
                bail!(
                    "reference evaluator exceeded its call-recursion ceiling of \
                     {MAX_EVAL_CALL_DEPTH} nested calls (deep or non-terminating recursion); \
                     this is an oracle robustness bound — the native backend evaluates on the \
                     OS stack and is unaffected"
                );
            }
            depth.set(next);
            Ok(EvalCallDepthGuard)
        })
    }
}

impl Drop for EvalCallDepthGuard {
    fn drop(&mut self) {
        EVAL_CALL_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::I8(left), Value::I8(right)) => left == right,
            (Value::I16(left), Value::I16(right)) => left == right,
            (Value::I32(left), Value::I32(right)) => left == right,
            (Value::I64(left), Value::I64(right)) => left == right,
            (Value::U8(left), Value::U8(right)) => left == right,
            (Value::U16(left), Value::U16(right)) => left == right,
            (Value::U32(left), Value::U32(right)) => left == right,
            (Value::U64(left), Value::U64(right)) => left == right,
            (Value::Bool(left), Value::Bool(right)) => left == right,
            (Value::Unit, Value::Unit) => true,
            (Value::SharedRef(left), Value::SharedRef(right))
            | (Value::MutRef(left), Value::MutRef(right)) => *left.borrow() == *right.borrow(),
            (
                Value::RawPtr {
                    target: left,
                    mutable: left_mutable,
                },
                Value::RawPtr {
                    target: right,
                    mutable: right_mutable,
                },
            ) => left_mutable == right_mutable && Rc::ptr_eq(left, right),
            (Value::Boxed(left), Value::Boxed(right)) => *left.borrow() == *right.borrow(),
            (
                Value::Slice {
                    elements: left,
                    mutable: left_mutable,
                },
                Value::Slice {
                    elements: right,
                    mutable: right_mutable,
                },
            ) => {
                left_mutable == right_mutable
                    && left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| *left.borrow() == *right.borrow())
            }
            (
                Value::Vec {
                    elements: left,
                    capacity: left_capacity,
                },
                Value::Vec {
                    elements: right,
                    capacity: right_capacity,
                },
            ) => {
                left_capacity == right_capacity
                    && left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| *left.borrow() == *right.borrow())
            }
            (Value::String(left), Value::String(right)) => left == right,
            (Value::Array(left), Value::Array(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| *left.borrow() == *right.borrow())
            }
            (Value::Record(left), Value::Record(right)) => {
                left.len() == right.len()
                    && left.iter().all(|(name, left)| {
                        right
                            .get(name)
                            .is_some_and(|right| *left.borrow() == *right.borrow())
                    })
            }
            (
                Value::Enum {
                    variant: left_variant,
                    value: left_value,
                },
                Value::Enum {
                    variant: right_variant,
                    value: right_value,
                },
            ) => left_variant == right_variant && *left_value.borrow() == *right_value.borrow(),
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::I8(value) => write!(f, "{value}"),
            Value::I16(value) => write!(f, "{value}"),
            Value::I32(value) => write!(f, "{value}"),
            Value::I64(value) => write!(f, "{value}"),
            Value::U8(value) => write!(f, "{value}"),
            Value::U16(value) => write!(f, "{value}"),
            Value::U32(value) => write!(f, "{value}"),
            Value::U64(value) => write!(f, "{value}"),
            Value::Bool(value) => write!(f, "{value}"),
            Value::Unit => write!(f, "()"),
            Value::SharedRef(value) => write!(f, "&{}", value.borrow()),
            Value::MutRef(value) => write!(f, "&mut {}", value.borrow()),
            Value::RawPtr { mutable, .. } => {
                if *mutable {
                    write!(f, "raw_mut_ptr(...)")
                } else {
                    write!(f, "raw_ptr(...)")
                }
            }
            Value::Boxed(value) => write!(f, "box({})", value.borrow()),
            Value::Slice { elements, mutable } => {
                let rendered = elements
                    .iter()
                    .map(|value| value.borrow().to_string())
                    .collect::<Vec<_>>();
                if *mutable {
                    write!(f, "mut_slice[{}]", rendered.join(", "))
                } else {
                    write!(f, "slice[{}]", rendered.join(", "))
                }
            }
            Value::Vec { elements, capacity } => {
                let rendered = elements
                    .iter()
                    .map(|value| value.borrow().to_string())
                    .collect::<Vec<_>>();
                write!(f, "vec(capacity: {capacity})[{}]", rendered.join(", "))
            }
            Value::String(bytes) => match String::from_utf8(bytes.clone()) {
                Ok(value) => write!(f, "\"{}\"", source_string_literal(&value)),
                Err(_) => write!(f, "string({} bytes)", bytes.len()),
            },
            Value::Array(elements) => {
                let rendered = elements
                    .iter()
                    .map(|value| value.borrow().to_string())
                    .collect::<Vec<_>>();
                write!(f, "[{}]", rendered.join(", "))
            }
            Value::Record(fields) => {
                let rendered = fields
                    .iter()
                    .map(|(name, value)| format!("{name}: {}", value.borrow()))
                    .collect::<Vec<_>>();
                write!(f, "{{{}}}", rendered.join(", "))
            }
            Value::Enum { variant, value } => {
                if matches!(*value.borrow(), Value::Unit) {
                    write!(f, "{variant}")
                } else {
                    write!(f, "{variant}({})", value.borrow())
                }
            }
        }
    }
}

impl CodeDb {
    pub(crate) fn eval_name(
        &self,
        root_hash: &str,
        function_name: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        let symbol = self.resolve_symbol_or_name(root_hash, function_name)?;
        self.eval_symbol(root_hash, &symbol, args)
    }

    pub(crate) fn eval_symbol(
        &self,
        root_hash: &str,
        symbol: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        // Bound host-stack recursion so a deep / non-terminating program yields a
        // clean error instead of overflowing the stack and aborting the process
        // (see `MAX_EVAL_CALL_DEPTH`). The guard decrements on return.
        let _call_depth = EvalCallDepthGuard::enter()?;
        let root = self.load_root(root_hash)?;
        let root_symbol = self
            .root_symbol(&root, symbol)
            .ok_or_else(|| anyhow!("missing symbol {symbol}"))?;
        let (param_types, _) = self.signature_parts(&root_symbol.signature)?;
        if param_types.len() != args.len() {
            bail!(
                "{} expects {} args, got {}",
                self.symbol_display(&root, symbol)?,
                param_types.len(),
                args.len()
            );
        }
        for (idx, (arg, ty)) in args.iter().zip(param_types.iter()).enumerate() {
            if !self.value_has_type(&root, arg, ty)? {
                bail!(
                    "argument {idx} has wrong type for {}: expected {}, got {arg}",
                    self.symbol_display(&root, symbol)?,
                    self.type_name(ty)?,
                );
            }
        }
        if self.definition_is_external(&root_symbol.definition)? {
            bail!(
                "cannot evaluate external function {}",
                self.symbol_display(&root, symbol)?
            );
        }
        let body = self.function_body_hash(&root_symbol.definition)?;
        let mut args = args.into_iter().map(value_cell).collect::<Vec<_>>();
        // This is the early-return (R7) boundary: an early `return` inside the body
        // unwinds here as a sentinel error, which becomes this call's result. Any
        // other error propagates unchanged.
        match self.eval_expr(root_hash, &body, &mut args) {
            Ok(value) => Ok(value),
            Err(err) => take_return_unwind(err),
        }
    }

    pub(crate) fn eval_expr(
        &self,
        root_hash: &str,
        expr_hash: &str,
        args: &mut Vec<ValueCell>,
    ) -> Result<Value> {
        self.eval_expr_with_locals(root_hash, expr_hash, args, &mut Vec::new())
    }

    pub(crate) fn static_data_bytes_hex(&self, data_hash: &str) -> Result<String> {
        if self.get_kind(data_hash)? != "StaticData" {
            bail!("static data hash points to non-StaticData object {data_hash}");
        }
        let payload = self.get_payload(data_hash)?;
        if payload.get("schema").and_then(JsonValue::as_str) != Some("codedb/static-data/v1") {
            bail!("static data object has unsupported schema");
        }
        let bytes_hex = payload
            .get("bytes_hex")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("StaticData missing bytes_hex"))?
            .to_string();
        let len = payload
            .get("len")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("StaticData missing len"))? as usize;
        let expected = static_data_payload(&bytes_hex, len)?;
        if canonical_json(&expected) != canonical_json(&payload) {
            bail!("StaticData payload is not canonical");
        }
        Ok(bytes_hex)
    }

    pub(crate) fn static_data_bytes(&self, data_hash: &str) -> Result<Vec<u8>> {
        hex_to_bytes(&self.static_data_bytes_hex(data_hash)?)
    }

    pub(crate) fn value_has_type(
        &self,
        root: &ProgramRootPayload,
        value: &Value,
        type_hash: &str,
    ) -> Result<bool> {
        match (value, self.type_spec_in_root(root, type_hash)?) {
            // A generic parameter `T` (R11) is type-erased at evaluation: the
            // reference evaluator runs the generic body on whatever concrete
            // value the caller supplied, so any value inhabits a `TypeParam`.
            (_, TypeSpec::TypeParam { .. }) => Ok(true),
            (Value::I8(_), TypeSpec::Builtin(kind)) => Ok(kind == "I8"),
            (Value::I16(_), TypeSpec::Builtin(kind)) => Ok(kind == "I16"),
            (Value::I32(_), TypeSpec::Builtin(kind)) => Ok(kind == "I32"),
            (Value::I64(_), TypeSpec::Builtin(kind)) => Ok(kind == "I64"),
            (Value::U8(_), TypeSpec::Builtin(kind)) => Ok(kind == "U8"),
            (Value::U16(_), TypeSpec::Builtin(kind)) => Ok(kind == "U16"),
            (Value::U32(_), TypeSpec::Builtin(kind)) => Ok(kind == "U32"),
            (Value::U64(_), TypeSpec::Builtin(kind)) => Ok(kind == "U64"),
            (Value::Bool(_), TypeSpec::Builtin(kind)) => Ok(kind == "Bool"),
            (Value::Unit, TypeSpec::Builtin(kind)) => Ok(kind == "Unit"),
            (
                Value::SharedRef(value),
                TypeSpec::Reference {
                    mutable: false,
                    referent,
                    ..
                },
            ) => self.value_has_type(root, &value.borrow(), &referent),
            (
                Value::MutRef(value),
                TypeSpec::Reference {
                    mutable: true,
                    referent,
                    ..
                },
            ) => self.value_has_type(root, &value.borrow(), &referent),
            (
                Value::RawPtr { target, mutable },
                TypeSpec::RawPointer {
                    mutable: expected_mutable,
                    pointee,
                },
            ) => Ok((*mutable || !expected_mutable)
                && self.value_has_type(root, &target.borrow(), &pointee)?),
            (Value::Boxed(value), TypeSpec::Box { element }) => {
                self.value_has_type(root, &value.borrow(), &element)
            }
            (Value::Vec { elements, .. }, TypeSpec::Vec { element }) => {
                for value in elements {
                    if !self.value_has_type(root, &value.borrow(), &element)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (Value::String(_), TypeSpec::String) => Ok(true),
            (
                Value::Slice { elements, mutable },
                TypeSpec::Slice {
                    mutable: expected_mutable,
                    element,
                    ..
                },
            ) => {
                if *mutable != expected_mutable {
                    return Ok(false);
                }
                for value in elements {
                    if !self.value_has_type(root, &value.borrow(), &element)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (Value::Array(values), TypeSpec::FixedArray { element, len }) => {
                if values.len() as u64 != len {
                    return Ok(false);
                }
                for value in values {
                    if !self.value_has_type(root, &value.borrow(), &element)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (Value::Record(values), TypeSpec::Record(fields)) => {
                if values.len() != fields.len() {
                    return Ok(false);
                }
                for field in fields {
                    let Some(value) = values.get(&field.name) else {
                        return Ok(false);
                    };
                    if !self.value_has_type(root, &value.borrow(), &field.type_hash)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (Value::Enum { variant, value }, TypeSpec::Enum(variants)) => {
                let Some(variant) = variants.iter().find(|candidate| candidate.name == *variant)
                else {
                    return Ok(false);
                };
                self.value_has_type(root, &value.borrow(), &variant.type_hash)
            }
            _ => Ok(false),
        }
    }

    /// Evaluate `loop acc = init while cond do body` (R8). Kept out of
    /// `eval_expr_with_locals` (and never inlined) so its locals do not enlarge that
    /// hot recursive frame. acc starts at init; while cond(acc) holds, acc becomes
    /// body(acc); the loop yields the final acc. `cond` and `body` read `acc` from
    /// one shared cell per iteration; a generous iteration ceiling converts a
    /// non-terminating loop into a clean error (an oracle bound, not a native limit).
    #[inline(never)]
    fn eval_loop(
        &self,
        root_hash: &str,
        payload: &JsonValue,
        args: &mut Vec<ValueCell>,
        locals: &mut Vec<ValueCell>,
    ) -> Result<Value> {
        let init_hash = payload
            .get("init")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("loop missing init"))?;
        let cond_hash = payload
            .get("cond")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("loop missing cond"))?;
        let body_hash = payload
            .get("body")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("loop missing body"))?;
        let mut accumulator = self.eval_expr_with_locals(root_hash, init_hash, args, locals)?;
        let mut iterations: u64 = 0;
        loop {
            locals.push(value_cell(semantic_clone_value(&accumulator)));
            let cond = self.eval_expr_with_locals(root_hash, cond_hash, args, locals);
            let cond = match cond {
                Ok(cond) => cond,
                Err(err) => {
                    locals.pop();
                    return Err(err);
                }
            };
            match cond {
                Value::Bool(false) => {
                    locals.pop();
                    break;
                }
                Value::Bool(true) => {}
                other => {
                    locals.pop();
                    bail!("loop condition evaluated to non-bool {other}");
                }
            }
            let next = self.eval_expr_with_locals(root_hash, body_hash, args, locals);
            locals.pop();
            accumulator = next?;
            iterations += 1;
            if iterations > MAX_EVAL_LOOP_ITERATIONS {
                bail!(
                    "loop exceeded the reference evaluator's {MAX_EVAL_LOOP_ITERATIONS}-iteration ceiling (likely non-terminating); it is an oracle bound, not a native limit"
                );
            }
        }
        Ok(accumulator)
    }

    /// `array_set(arr, i, v)`: clone `arr` (a Copy array) and overwrite element `i`
    /// with `v`, yielding the new array — the functional update the native backend
    /// lowers to copy-then-store. In an `#[inline(never)]` helper (like
    /// `eval_string_builtin`) so its locals never inflate the hot `eval_expr_with_locals`
    /// frame and shrink the depth the evaluator reaches before the host stack overflows.
    #[inline(never)]
    fn eval_array_set(
        &self,
        root_hash: &str,
        payload: &JsonValue,
        args: &mut Vec<ValueCell>,
        locals: &mut Vec<ValueCell>,
    ) -> Result<Value> {
        let array_hash = payload
            .get("array")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("array_set missing array"))?;
        let index_hash = payload
            .get("index")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("array_set missing index"))?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("array_set missing value"))?;
        let array = self.eval_expr_with_locals(root_hash, array_hash, args, locals)?;
        let index =
            eval_index_value(self.eval_expr_with_locals(root_hash, index_hash, args, locals)?)?;
        let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
        let Value::Array(mut cells) = semantic_clone_value(&array) else {
            bail!("array_set target evaluated to a non-array value");
        };
        if index >= cells.len() {
            bail!(
                "array_set index {index} out of bounds for length {}",
                cells.len()
            );
        }
        cells[index] = value_cell(value);
        Ok(Value::Array(cells))
    }

    /// The dynamic-string builtins (`string_with_capacity`, `string_push`,
    /// `string_get`), factored out of `eval_expr_with_locals` and marked
    /// `#[inline(never)]` so their locals never inflate that hot recursive frame
    /// (which would lower the depth the evaluator reaches before the host stack
    /// overflows). A `string` is modeled as a growable byte buffer; the native
    /// backend enforces the fixed capacity, an edge a correctly-sized program
    /// never reaches.
    #[inline(never)]
    fn eval_string_builtin(
        &self,
        root_hash: &str,
        kind: &str,
        payload: &JsonValue,
        args: &mut Vec<ValueCell>,
        locals: &mut Vec<ValueCell>,
    ) -> Result<Value> {
        match kind {
            "string_with_capacity" => {
                let capacity_hash = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_with_capacity missing capacity"))?;
                // Evaluate the capacity for effect/error parity, then discard it.
                let _ = eval_index_value(self.eval_expr_with_locals(
                    root_hash,
                    capacity_hash,
                    args,
                    locals,
                )?)?;
                Ok(Value::String(Vec::new()))
            }
            "string_push" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_push missing target"))?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_push missing value"))?;
                let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                let byte = match value {
                    Value::U8(byte) => byte,
                    other => bail!("string_push value evaluated to non-u8 {other}"),
                };
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                match &mut *target.borrow_mut() {
                    Value::String(bytes) => {
                        bytes.push(byte);
                        Ok(Value::Unit)
                    }
                    other => bail!("string_push target evaluated to non-string {other}"),
                }
            }
            "string_get" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_get missing target"))?;
                let index_hash = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_get missing index"))?;
                let index = eval_index_value(self.eval_expr_with_locals(
                    root_hash,
                    index_hash,
                    args,
                    locals,
                )?)?;
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                match &*target.borrow() {
                    Value::String(bytes) => bytes
                        .get(index)
                        .map(|byte| Value::U8(*byte))
                        .ok_or_else(|| anyhow!("string_get index {index} out of bounds")),
                    other => bail!("string_get target evaluated to non-string {other}"),
                }
            }
            other => bail!("eval_string_builtin called with non-string builtin {other}"),
        }
    }

    fn eval_expr_with_locals(
        &self,
        root_hash: &str,
        expr_hash: &str,
        args: &mut Vec<ValueCell>,
        locals: &mut Vec<ValueCell>,
    ) -> Result<Value> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => {
                // An integer literal of the width given by its `type` field
                // (context-typed; defaults to i64). See `literal_int_type`.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?;
                let type_hash = payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing type"))?;
                let TypeSpec::Builtin(name) = self.type_spec(type_hash)? else {
                    bail!("integer literal has non-builtin type");
                };
                let int = crate::types::scalar_int_type(&name)
                    .ok_or_else(|| anyhow!("integer literal has non-integer type {name}"))?;
                int_literal_value(value, int)
            }
            "literal_bool" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?;
                Ok(Value::Bool(value))
            }
            "static_bytes" => {
                let data_hash = payload
                    .get("static_data")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing static_data"))?;
                let data = self.static_data_bytes(data_hash)?;
                Ok(Value::Slice {
                    elements: data.into_iter().map(Value::U8).map(value_cell).collect(),
                    mutable: false,
                })
            }
            "literal_unit" => Ok(Value::Unit),
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                args.get(index)
                    .map(|value| semantic_clone_value(&value.borrow()))
                    .ok_or_else(|| anyhow!("parameter index out of bounds: {index}"))
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                local_at_depth(locals, depth)
                    .map(|value| semantic_clone_value(&value.borrow()))
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))
            }
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let arg_hashes = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                let mut call_args = Vec::with_capacity(arg_hashes.len());
                for arg in arg_hashes {
                    let hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    call_args.push(self.eval_expr_with_locals(root_hash, hash, args, locals)?);
                }
                self.eval_symbol(root_hash, symbol, call_args)
            }
            "binary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?;
                let left_hash = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right_hash = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                let left = self.eval_expr_with_locals(root_hash, left_hash, args, locals)?;
                let right = self.eval_expr_with_locals(root_hash, right_hash, args, locals)?;
                eval_binary(op, left, right)
            }
            "unary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?;
                let expr_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?;
                let value = self.eval_expr_with_locals(root_hash, expr_hash, args, locals)?;
                eval_unary(op, value)
            }
            "borrow_shared" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing target"))?;
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                Ok(Value::SharedRef(box_payload_cell(&target)))
            }
            "borrow_mut" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing target"))?;
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                Ok(Value::MutRef(box_payload_cell(&target)))
            }
            "slice_from_array" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_from_array missing target"))?;
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("slice_from_array missing mutable"))?;
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                let elements = slice_cells_from_array_cell(&target)?;
                Ok(Value::Slice { elements, mutable })
            }
            "slice_len" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_len missing target"))?;
                let target = self.eval_expr_with_locals(root_hash, target_hash, args, locals)?;
                Ok(Value::I64(slice_len_from_value(&target)? as i64))
            }
            "subslice" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing target"))?;
                let start_hash = payload
                    .get("start")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing start"))?;
                let len_hash = payload
                    .get("len")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing len"))?;
                let target = self.eval_expr_with_locals(root_hash, target_hash, args, locals)?;
                let start = eval_index_value(
                    self.eval_expr_with_locals(root_hash, start_hash, args, locals)?,
                )?;
                let len = eval_index_value(
                    self.eval_expr_with_locals(root_hash, len_hash, args, locals)?,
                )?;
                subslice_value(&target, start, len)
            }
            "box_new" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("box_new missing value"))?;
                Ok(Value::Boxed(value_cell(self.eval_expr_with_locals(
                    root_hash, value_hash, args, locals,
                )?)))
            }
            "unbox" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unbox missing value"))?;
                let boxed = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                match boxed {
                    Value::Boxed(cell) => Ok(cell.borrow().clone()),
                    _ => bail!("unbox expects a boxed value"),
                }
            }
            "int_cast" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing value"))?;
                let target_hash = payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing type"))?;
                let target = crate::types::scalar_int_type_by_hash(target_hash)
                    .ok_or_else(|| anyhow!("int_cast target is not a sized integer"))?;
                let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                cast_int_value(&value, target)
            }
            "vec_new" => {
                let capacity_hash = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_new missing capacity"))?;
                let capacity = eval_index_value(self.eval_expr_with_locals(
                    root_hash,
                    capacity_hash,
                    args,
                    locals,
                )?)?;
                Ok(Value::Vec {
                    elements: Vec::with_capacity(capacity),
                    capacity,
                })
            }
            "vec_push" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_push missing target"))?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_push missing value"))?;
                let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                match &mut *target.borrow_mut() {
                    Value::Vec { elements, capacity } => {
                        if elements.len() >= *capacity {
                            bail!("vec_push capacity {} exceeded", capacity);
                        }
                        elements.push(value_cell(value));
                        Ok(Value::Unit)
                    }
                    other => bail!("vec_push target evaluated to non-vec {other}"),
                }
            }
            "vec_get" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_get missing target"))?;
                let index_hash = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_get missing index"))?;
                let index = eval_index_value(
                    self.eval_expr_with_locals(root_hash, index_hash, args, locals)?,
                )?;
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                match &*target.borrow() {
                    Value::Vec { elements, .. } => elements
                        .get(index)
                        .map(|value| semantic_clone_value(&value.borrow()))
                        .ok_or_else(|| anyhow!("vec_get index {index} out of bounds")),
                    other => bail!("vec_get target evaluated to non-vec {other}"),
                }
            }
            "vec_len" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_len missing target"))?;
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                match &*target.borrow() {
                    Value::Vec { elements, .. } => Ok(Value::I64(elements.len() as i64)),
                    other => bail!("vec_len target evaluated to non-vec {other}"),
                }
            }
            "string_new" => {
                let source_hash = payload
                    .get("source")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_new missing source"))?;
                let source = self.eval_expr_with_locals(root_hash, source_hash, args, locals)?;
                Ok(Value::String(bytes_from_slice_value(&source)?))
            }
            "string_len" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_len missing target"))?;
                let target = self.eval_place_cell(root_hash, target_hash, args, locals)?;
                match &*target.borrow() {
                    Value::String(bytes) => Ok(Value::I64(bytes.len() as i64)),
                    other => bail!("string_len target evaluated to non-string {other}"),
                }
            }
            // The dynamic-string builtins live in an #[inline(never)] helper so their
            // locals stay off this hot recursive frame — otherwise they shrink the
            // evaluator's effective recursion depth before the host stack overflows
            // (see eval_loop and MAX_EVAL_CALL_DEPTH).
            kind @ ("string_with_capacity" | "string_push" | "string_get") => {
                self.eval_string_builtin(root_hash, kind, &payload, args, locals)
            }
            "raw_ptr_cast" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?;
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing mutable"))?;
                match self.eval_expr_with_locals(root_hash, value_hash, args, locals)? {
                    Value::SharedRef(target) => {
                        if mutable {
                            bail!("cannot make raw mutable pointer from shared reference");
                        }
                        Ok(Value::RawPtr { target, mutable })
                    }
                    Value::MutRef(target) => Ok(Value::RawPtr { target, mutable }),
                    Value::RawPtr {
                        target,
                        mutable: source_mutable,
                    } => {
                        if mutable && !source_mutable {
                            bail!("cannot cast raw shared pointer to mutable");
                        }
                        Ok(Value::RawPtr { target, mutable })
                    }
                    other => bail!("raw_ptr cast evaluated non-pointer source {other}"),
                }
            }
            "raw_load" => {
                let pointer_hash = payload
                    .get("pointer")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_load missing pointer"))?;
                match self.eval_expr_with_locals(root_hash, pointer_hash, args, locals)? {
                    Value::RawPtr { target, .. } => Ok(semantic_clone_value(&target.borrow())),
                    other => bail!("raw_load evaluated non-pointer source {other}"),
                }
            }
            "raw_store" => {
                let pointer_hash = payload
                    .get("pointer")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_store missing pointer"))?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_store missing value"))?;
                let pointer = self.eval_expr_with_locals(root_hash, pointer_hash, args, locals)?;
                let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                match pointer {
                    Value::RawPtr {
                        target,
                        mutable: true,
                    } => {
                        *target.borrow_mut() = value;
                        Ok(Value::Unit)
                    }
                    Value::RawPtr { mutable: false, .. } => {
                        bail!("raw_store requires raw mutable pointer")
                    }
                    other => bail!("raw_store evaluated non-pointer source {other}"),
                }
            }
            "assign" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("assign missing target"))?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("assign missing value"))?;
                let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                *self
                    .eval_place_cell(root_hash, target_hash, args, locals)?
                    .borrow_mut() = value;
                Ok(Value::Unit)
            }
            "let" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing value"))?;
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing body"))?;
                let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                locals.push(value_cell(value));
                let body = self.eval_expr_with_locals(root_hash, body_hash, args, locals);
                locals.pop();
                body
            }
            "if" => {
                let cond_hash = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing cond"))?;
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                match self.eval_expr_with_locals(root_hash, cond_hash, args, locals)? {
                    Value::Bool(true) => {
                        self.eval_expr_with_locals(root_hash, then_hash, args, locals)
                    }
                    Value::Bool(false) => {
                        self.eval_expr_with_locals(root_hash, else_hash, args, locals)
                    }
                    other => bail!("if condition evaluated to non-bool {other}"),
                }
            }
            "return" => {
                // Early exit (R7): evaluate the operand, then unwind to the
                // enclosing function-call boundary (`eval_symbol`) by raising a
                // sentinel error carrying the value out-of-band (a `Value` holds an
                // `Rc` and cannot ride inside a `Send + Sync` error). `?`-propagation
                // through `let`/`if`/`case` runs their scope cleanup (e.g. the `let`
                // pops its local before returning the error) and then unwinds.
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("return missing value"))?;
                let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                Err(raise_return_unwind(value))
            }
            "fold" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing target"))?;
                let init_hash = payload
                    .get("init")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing init"))?;
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing body"))?;
                let target = self.eval_expr_with_locals(root_hash, target_hash, args, locals)?;
                let elements = match target {
                    Value::Array(elements) | Value::Slice { elements, .. } => elements,
                    other => bail!("fold target is not an array or slice: {other}"),
                };
                let mut accumulator =
                    self.eval_expr_with_locals(root_hash, init_hash, args, locals)?;
                for item in elements {
                    locals.push(value_cell(semantic_clone_value(&item.borrow())));
                    locals.push(value_cell(accumulator));
                    let next = self.eval_expr_with_locals(root_hash, body_hash, args, locals);
                    locals.pop();
                    locals.pop();
                    accumulator = next?;
                }
                Ok(accumulator)
            }
            // Extracted (and never inlined) so this big arm's locals do NOT enlarge
            // the hot, deeply-recursive `eval_expr_with_locals` stack frame — which
            // would push the host-stack recursion ceiling past the real overflow
            // depth (the `loop` body is never on the deep-recursion path).
            "loop" => self.eval_loop(root_hash, &payload, args, locals),
            "array_literal" => {
                let mut values = Vec::new();
                for element in payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                {
                    let value_hash = element
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing value"))?;
                    values.push(value_cell(
                        self.eval_expr_with_locals(root_hash, value_hash, args, locals)?,
                    ));
                }
                Ok(Value::Array(values))
            }
            "array_fill" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                let count = payload
                    .get("count")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("array_fill missing count"))?;
                // Evaluate the value ONCE, then replicate (the type rule guarantees a
                // Copy value, so the clones are independent).
                let value = self.eval_expr_with_locals(root_hash, value_hash, args, locals)?;
                let mut values = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    values.push(value_cell(semantic_clone_value(&value)));
                }
                Ok(Value::Array(values))
            }
            // In an `#[inline(never)]` helper so its locals never inflate this hot
            // recursive frame (which would lower the depth the evaluator reaches
            // before the host stack overflows — the documented eval-frame gotcha).
            "array_set" => self.eval_array_set(root_hash, &payload, args, locals),
            "array_index" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let index_hash = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing index"))?;
                match self.eval_place_cell(root_hash, target_hash, args, locals) {
                    Ok(target) => {
                        let index = eval_index_value(
                            self.eval_expr_with_locals(root_hash, index_hash, args, locals)?,
                        )?;
                        Ok(semantic_clone_value(&array_cell(&target, index)?.borrow()))
                    }
                    Err(_) => {
                        let target =
                            self.eval_expr_with_locals(root_hash, target_hash, args, locals)?;
                        let index = eval_index_value(
                            self.eval_expr_with_locals(root_hash, index_hash, args, locals)?,
                        )?;
                        Ok(semantic_clone_value(
                            &array_cell_from_value(&target, index)?.borrow(),
                        ))
                    }
                }
            }
            "record_literal" => {
                let mut values = BTreeMap::new();
                for field in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                {
                    let name = field
                        .get("name")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing name"))?
                        .to_string();
                    let value_hash = field
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing value"))?;
                    values.insert(
                        name,
                        value_cell(
                            self.eval_expr_with_locals(root_hash, value_hash, args, locals)?,
                        ),
                    );
                }
                Ok(Value::Record(values))
            }
            "field_access" => {
                let value = self.eval_place_cell(root_hash, expr_hash, args, locals)?;
                Ok(semantic_clone_value(&value.borrow()))
            }
            "enum_construct" => {
                let variant = payload
                    .get("variant")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing variant"))?
                    .to_string();
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                Ok(Value::Enum {
                    variant,
                    value: value_cell(
                        self.eval_expr_with_locals(root_hash, value_hash, args, locals)?,
                    ),
                })
            }
            "case" => {
                let expr_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                let value = self.eval_expr_with_locals(root_hash, expr_hash, args, locals)?;
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?;
                match value {
                    Value::Enum { variant, value } => {
                        // First-match over the arms: an arm is taken when its (possibly
                        // nested, R14) pattern matches the scrutinee, binding the
                        // chain's leaf; a `default` arm matches anything. Mirrors the
                        // decision-tree lowering.
                        let scrutinee_cell: ValueCell =
                            Rc::new(RefCell::new(Value::Enum { variant, value }));
                        let mut selected: Option<(&JsonValue, Option<ValueCell>)> = None;
                        for arm in arms {
                            if typed_case_arm_is_default(arm) {
                                selected = Some((arm, None));
                                break;
                            }
                            if let Some(leaf) = match_typed_pattern(arm, &scrutinee_cell) {
                                selected = Some((arm, leaf));
                                break;
                            }
                        }
                        let (arm, leaf) = selected.ok_or_else(|| {
                            let variant = match &*scrutinee_cell.borrow() {
                                Value::Enum { variant, .. } => variant.clone(),
                                _ => String::new(),
                            };
                            anyhow!("case missing arm for variant {variant}")
                        })?;
                        let body_hash = arm
                            .get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?;
                        if let Some(leaf) = leaf {
                            locals.push(leaf);
                            let result =
                                self.eval_expr_with_locals(root_hash, body_hash, args, locals);
                            locals.pop();
                            result
                        } else {
                            self.eval_expr_with_locals(root_hash, body_hash, args, locals)
                        }
                    }
                    // Scalar `i64` `case` (R14): first-match over the arms in source
                    // order — an arm is taken when its pattern matches AND its `if`
                    // guard (if present) evaluates true; otherwise control falls
                    // through to the next arm. The unguarded `_` wildcard is last and
                    // always matches. This mirrors the native `if`/`eq` chain.
                    Value::I64(scrutinee) => {
                        let mut selected: Option<&JsonValue> = None;
                        for arm in arms {
                            let matches = typed_case_arm_is_default(arm)
                                || scalar_i64_arm_matches(arm, scrutinee);
                            if !matches {
                                continue;
                            }
                            if let Some(guard_hash) = arm.get("guard").and_then(JsonValue::as_str)
                            {
                                match self
                                    .eval_expr_with_locals(root_hash, guard_hash, args, locals)?
                                {
                                    Value::Bool(true) => {}
                                    Value::Bool(false) => continue,
                                    other => {
                                        bail!("case guard must evaluate to bool, got {other}")
                                    }
                                }
                            }
                            selected = Some(arm);
                            break;
                        }
                        let arm = selected.ok_or_else(|| {
                            anyhow!("scalar case missing arm for value {scrutinee}")
                        })?;
                        let body_hash = arm
                            .get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?;
                        self.eval_expr_with_locals(root_hash, body_hash, args, locals)
                    }
                    Value::Bool(scrutinee) => {
                        let arm = arms
                            .iter()
                            .find(|arm| {
                                arm.get("literal_bool").and_then(JsonValue::as_bool)
                                    == Some(scrutinee)
                            })
                            .or_else(|| arms.iter().find(|arm| typed_case_arm_is_default(arm)))
                            .ok_or_else(|| anyhow!("scalar case missing arm for value {scrutinee}"))?;
                        let body_hash = arm
                            .get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?;
                        self.eval_expr_with_locals(root_hash, body_hash, args, locals)
                    }
                    other => bail!("case expression evaluated to non-enum/scalar {other}"),
                }
            }
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn eval_place_cell(
        &self,
        root_hash: &str,
        expr_hash: &str,
        args: &mut Vec<ValueCell>,
        locals: &mut Vec<ValueCell>,
    ) -> Result<ValueCell> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                args.get_mut(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("parameter index out of bounds: {index}"))
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                local_at_depth_mut(locals, depth)
                    .cloned()
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))
            }
            "field_access" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                let field = payload
                    .get("field")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing field"))?;
                let target = self.eval_place_cell(root_hash, target, args, locals)?;
                let target = box_payload_cell(&target);
                field_cell(&target, field)
            }
            "array_index" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing index"))?;
                let target = self.eval_place_cell(root_hash, target, args, locals)?;
                let index =
                    eval_index_value(self.eval_expr_with_locals(root_hash, index, args, locals)?)?;
                array_cell(&target, index)
            }
            other => bail!("expression kind {other} is not an assignable place"),
        }
    }

    pub(crate) fn render_source(&self, root_hash: &str) -> Result<String> {
        let root = self.load_root(root_hash)?;
        let mut chunks = Vec::new();
        let has_non_main_modules = root_module_names(&root)
            .iter()
            .any(|name| name != MAIN_BRANCH);
        for binding in self.type_projection_order(&root)? {
            let type_entry = self.root_type(&root, &binding.type_symbol).ok_or_else(|| {
                anyhow!(
                    "root type name points to missing type {}",
                    binding.type_symbol
                )
            })?;
            let source = self.render_type_source(&root, &binding, type_entry)?;
            if has_non_main_modules && binding.module != MAIN_BRANCH {
                chunks.push(format!("module {} {{\n{}\n}}", binding.module, source));
            } else {
                chunks.push(source);
            }
        }
        for binding in self.source_projection_order(&root)? {
            let symbol = binding.symbol.clone();
            let root_symbol = self
                .root_symbol(&root, &symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {symbol}"))?;
            let source = self.render_function_source(&root, &binding, root_symbol)?;
            if has_non_main_modules && binding.module != MAIN_BRANCH {
                chunks.push(format!("module {} {{\n{}\n}}", binding.module, source));
            } else {
                chunks.push(source);
            }
        }
        Ok(format!("{}\n", chunks.join("\n\n")))
    }

    pub(crate) fn render_function_source(
        &self,
        root: &ProgramRootPayload,
        binding: &NameBinding,
        root_symbol: &RootSymbolPayload,
    ) -> Result<String> {
        if self.definition_is_external(&root_symbol.definition)? {
            let external = self.external_function_metadata(&root_symbol.definition)?;
            let mut source = format!(
                "extern fn {}{} link_name \"{}\"",
                binding.display_name,
                self.external_signature_source_in_root(
                    root,
                    &binding.module,
                    &root_symbol.signature,
                    &param_names(root, &binding.symbol),
                    &external.abi,
                )?,
                source_string_literal(&external.link_name),
            );
            if let Some(library) = external.library {
                source.push_str(&format!(" library \"{}\"", source_string_literal(&library)));
            }
            return Ok(source);
        }
        let body = self.function_body_hash(&root_symbol.definition)?;
        let region_names =
            signature_region_name_map(&self.signature_region_params(&root_symbol.signature)?);
        Ok(format!(
            "fn {}{} = {}",
            binding.display_name,
            self.signature_source_in_root(
                root,
                &binding.module,
                &root_symbol.signature,
                &param_names(root, &binding.symbol),
            )?,
            self.expr_to_source_in_module_with_regions(
                &body,
                root,
                &binding.module,
                &param_names(root, &binding.symbol),
                &region_names,
                0,
            )?
        ))
    }

    pub(crate) fn render_type_source(
        &self,
        root: &ProgramRootPayload,
        binding: &crate::model::TypeNameBinding,
        root_type: &crate::model::RootTypePayload,
    ) -> Result<String> {
        let definition = self.type_definition(&root_type.type_def)?;
        let type_symbol_birth = self.symbol_birth_spec(definition.type_symbol())?;
        // A type-recursion-group member re-derives its identity canonically on
        // re-import (its birth nonce is `type_recursion_group:{ordinal}`), exactly
        // like a function recursion-group member — so it must NOT emit identity pins.
        // Otherwise the re-imported clique op would carry pins the original lacked,
        // changing the op (and every downstream symbol's parent history) and breaking
        // the import→export→import fixpoint (SPEC_V3 §6/§11).
        let clique_member = type_symbol_birth
            .local_nonce
            .starts_with("type_recursion_group:");
        let type_identity = TypeDefinitionIdentity {
            type_symbol_birth,
            region_param_births: definition
                .region_params()
                .iter()
                .map(|param| self.symbol_birth_spec(&param.region))
                .collect::<Result<Vec<_>>>()?,
            member_births: Vec::new(),
        };
        let type_identity_prefix = if clique_member {
            String::new()
        } else {
            format!(
                "// codedb:type_identity {}\n",
                canonical_json(&serde_json::to_value(&type_identity)?)
            )
        };
        let member_identity_prefix = |spec: &str| -> String {
            if clique_member {
                String::new()
            } else {
                format!("  // codedb:member_identity {spec}\n")
            }
        };
        let region_names = definition
            .region_params()
            .iter()
            .map(|param| (param.region.clone(), param.name.clone()))
            .collect::<BTreeMap<_, _>>();
        let type_param_names = definition
            .type_params()
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        // Header parameter list `<'r, T>`: region parameters first, then type
        // parameters (R11) — the order `parse_optional_region_and_type_params`
        // accepts, so the projection re-parses to the same definition.
        let param_suffix =
            if definition.region_params().is_empty() && definition.type_params().is_empty() {
                String::new()
            } else {
                let parts = definition
                    .region_params()
                    .iter()
                    .map(|param| format!("'{}", param.name))
                    .chain(type_param_names.iter().cloned())
                    .collect::<Vec<_>>();
                format!("<{}>", parts.join(", "))
            };
        let render_member = |this: &Self, member: &crate::types::TypeMemberDef| -> Result<String> {
            let member_identity = canonical_json(&serde_json::to_value(
                this.symbol_birth_spec(&member.member_symbol)?,
            )?);
            Ok(format!(
                "{}  {}: {}",
                member_identity_prefix(&member_identity),
                member.name,
                this.type_name_in_root_with_scope(
                    root,
                    &binding.module,
                    &member.type_hash,
                    &region_names,
                    &type_param_names,
                )?
            ))
        };
        match definition {
            TypeDefinition::Record { fields, .. } => {
                let rendered_fields = fields
                    .iter()
                    .map(|field| render_member(self, field))
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!(
                    "{}record {}{} {{\n{}\n}}",
                    type_identity_prefix,
                    binding.display_name,
                    param_suffix,
                    rendered_fields.join("\n")
                ))
            }
            TypeDefinition::Enum { variants, .. } => {
                let rendered_variants = variants
                    .iter()
                    .map(|variant| render_member(self, variant))
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!(
                    "{}enum {}{} {{\n{}\n}}",
                    type_identity_prefix,
                    binding.display_name,
                    param_suffix,
                    rendered_variants.join("\n")
                ))
            }
        }
    }

    fn source_projection_order(
        &self,
        root: &ProgramRootPayload,
    ) -> Result<Vec<crate::model::NameBinding>> {
        let bindings = preferred_names(root);
        let binding_by_symbol = bindings
            .iter()
            .map(|binding| (binding.symbol.clone(), binding.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut ordered = Vec::new();

        for binding in bindings {
            self.visit_projection_symbol(
                root,
                &binding_by_symbol,
                &binding.symbol,
                &mut visiting,
                &mut visited,
                &mut ordered,
            )?;
        }

        Ok(ordered)
    }

    fn type_projection_order(
        &self,
        root: &ProgramRootPayload,
    ) -> Result<Vec<crate::model::TypeNameBinding>> {
        let bindings = preferred_type_names(root);
        let binding_by_type = bindings
            .iter()
            .map(|binding| (binding.type_symbol.clone(), binding.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut ordered = Vec::new();

        for binding in bindings {
            self.visit_projection_type(
                root,
                &binding_by_type,
                &binding.type_symbol,
                &mut visiting,
                &mut visited,
                &mut ordered,
            )?;
        }
        Ok(ordered)
    }

    fn visit_projection_type(
        &self,
        root: &ProgramRootPayload,
        binding_by_type: &BTreeMap<String, crate::model::TypeNameBinding>,
        type_symbol: &str,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        ordered: &mut Vec<crate::model::TypeNameBinding>,
    ) -> Result<()> {
        if visited.contains(type_symbol) {
            return Ok(());
        }
        if !visiting.insert(type_symbol.to_string()) {
            return Ok(());
        }

        if let Some(entry) = self.root_type(root, type_symbol) {
            for dependency in self.dependencies_for_type_definition(root, &entry.type_def)? {
                if binding_by_type.contains_key(&dependency) {
                    self.visit_projection_type(
                        root,
                        binding_by_type,
                        &dependency,
                        visiting,
                        visited,
                        ordered,
                    )?;
                }
            }
        }

        visiting.remove(type_symbol);
        if visited.insert(type_symbol.to_string())
            && let Some(binding) = binding_by_type.get(type_symbol)
        {
            ordered.push(binding.clone());
        }
        Ok(())
    }

    fn visit_projection_symbol(
        &self,
        root: &ProgramRootPayload,
        binding_by_symbol: &std::collections::BTreeMap<String, crate::model::NameBinding>,
        symbol: &str,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        ordered: &mut Vec<crate::model::NameBinding>,
    ) -> Result<()> {
        if visited.contains(symbol) {
            return Ok(());
        }
        if !visiting.insert(symbol.to_string()) {
            return Ok(());
        }

        if let Some(entry) = self.root_symbol(root, symbol) {
            // Order by *named* dependencies so a generic call orders this
            // function after the generic it names (R11), not after the unnamed
            // instance — which a projection cannot emit.
            for dependency in self.named_dependencies_for_definition(root, &entry.definition)? {
                if binding_by_symbol.contains_key(&dependency) {
                    self.visit_projection_symbol(
                        root,
                        binding_by_symbol,
                        &dependency,
                        visiting,
                        visited,
                        ordered,
                    )?;
                }
            }
        }

        visiting.remove(symbol);
        if visited.insert(symbol.to_string())
            && let Some(binding) = binding_by_symbol.get(symbol)
        {
            ordered.push(binding.clone());
        }
        Ok(())
    }

    pub(crate) fn signature_source(
        &self,
        signature_hash: &str,
        param_names: &[String],
    ) -> Result<String> {
        let region_params = self.signature_region_params(signature_hash)?;
        let region_names = signature_region_name_map(&region_params);
        let (params, return_type) = self.signature_parts(signature_hash)?;
        let effects = self.signature_effects(signature_hash)?;
        let rendered_params = params
            .iter()
            .enumerate()
            .map(|(idx, ty)| {
                let name = param_names
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| format!("p{idx}"));
                Ok(format!(
                    "{name}: {}",
                    self.type_name_with_regions(ty, &region_names)?
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut source = format!(
            "{}({}) -> {}",
            signature_region_suffix(&region_params),
            rendered_params.join(", "),
            self.type_name_with_regions(&return_type, &region_names)?
        );
        if !effects.is_empty() {
            let rendered_effects = visible_effects(&effects)
                .into_iter()
                .map(|effect| effect.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            source.push_str(&format!(" effects[{rendered_effects}]"));
        }
        Ok(source)
    }

    #[allow(dead_code)]
    pub(crate) fn external_signature_source(
        &self,
        signature_hash: &str,
        param_names: &[String],
        abi: &str,
    ) -> Result<String> {
        let region_params = self.signature_region_params(signature_hash)?;
        let region_names = signature_region_name_map(&region_params);
        let (params, return_type) = self.signature_parts(signature_hash)?;
        let effects = self.signature_effects(signature_hash)?;
        let rendered_params = params
            .iter()
            .enumerate()
            .map(|(idx, ty)| {
                let name = param_names
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| format!("p{idx}"));
                Ok(format!(
                    "{name}: {}",
                    self.type_name_with_regions(ty, &region_names)?
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut source = format!(
            "{}({}) -> {} abi[{abi}]",
            signature_region_suffix(&region_params),
            rendered_params.join(", "),
            self.type_name_with_regions(&return_type, &region_names)?
        );
        if !effects.is_empty() {
            let rendered_effects = visible_effects(&effects)
                .into_iter()
                .map(|effect| effect.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            source.push_str(&format!(" effects[{rendered_effects}]"));
        }
        Ok(source)
    }

    pub(crate) fn signature_source_in_root(
        &self,
        root: &ProgramRootPayload,
        current_module: &str,
        signature_hash: &str,
        param_names: &[String],
    ) -> Result<String> {
        let region_params = self.signature_region_params(signature_hash)?;
        let region_names = signature_region_name_map(&region_params);
        // Generic functions (R11): the type-parameter names scope every
        // `TypeParam` in the parameter/return types and render in the `<...>`
        // header after the region parameters (the order the parser accepts).
        let type_param_names = self.signature_type_params(signature_hash)?;
        let (params, return_type) = self.signature_parts(signature_hash)?;
        let effects = self.signature_effects(signature_hash)?;
        let rendered_params = params
            .iter()
            .enumerate()
            .map(|(idx, ty)| {
                let name = param_names
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| format!("p{idx}"));
                Ok(format!(
                    "{name}: {}",
                    self.type_name_in_root_with_scope(
                        root,
                        current_module,
                        ty,
                        &region_names,
                        &type_param_names,
                    )?
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut source = format!(
            "{}({}) -> {}",
            signature_region_and_type_suffix(&region_params, &type_param_names),
            rendered_params.join(", "),
            self.type_name_in_root_with_scope(
                root,
                current_module,
                &return_type,
                &region_names,
                &type_param_names,
            )?
        );
        if !effects.is_empty() {
            let rendered_effects = visible_effects(&effects)
                .into_iter()
                .map(|effect| effect.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            source.push_str(&format!(" effects[{rendered_effects}]"));
        }
        Ok(source)
    }

    pub(crate) fn external_signature_source_in_root(
        &self,
        root: &ProgramRootPayload,
        current_module: &str,
        signature_hash: &str,
        param_names: &[String],
        abi: &str,
    ) -> Result<String> {
        let mut source =
            self.signature_source_in_root(root, current_module, signature_hash, param_names)?;
        let insert_at = source.find(" effects[").unwrap_or(source.len());
        source.insert_str(insert_at, &format!(" abi[{abi}]"));
        Ok(source)
    }

    pub(crate) fn expr_to_source(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        local_params: &[String],
        parent_prec: u8,
    ) -> Result<String> {
        self.expr_to_source_in_module(expr_hash, root, MAIN_BRANCH, local_params, parent_prec)
    }

    pub(crate) fn expr_to_source_in_module(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        local_params: &[String],
        parent_prec: u8,
    ) -> Result<String> {
        self.expr_to_source_in_module_with_regions(
            expr_hash,
            root,
            current_module,
            local_params,
            &BTreeMap::new(),
            parent_prec,
        )
    }

    fn expr_to_source_in_module_with_regions(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        local_params: &[String],
        region_names: &BTreeMap<String, String>,
        parent_prec: u8,
    ) -> Result<String> {
        self.expr_to_source_with_locals(
            expr_hash,
            root,
            current_module,
            local_params,
            region_names,
            &mut Vec::new(),
            parent_prec,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn expr_to_source_with_locals(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        local_params: &[String],
        region_names: &BTreeMap<String, String>,
        local_names: &mut Vec<String>,
        parent_prec: u8,
    ) -> Result<String> {
        let payload = self.get_payload(expr_hash)?;
        let rendered = match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => payload
                .get("value")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                .to_string(),
            "static_bytes" => {
                let literal_kind = payload
                    .get("literal_kind")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing literal_kind"))?;
                let bytes = self.static_data_bytes(
                    payload
                        .get("static_data")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("static_bytes missing static_data"))?,
                )?;
                match literal_kind {
                    "string" => {
                        let value = String::from_utf8(bytes)
                            .map_err(|_| anyhow!("string literal static data is not utf8"))?;
                        format!("\"{}\"", source_string_literal(&value))
                    }
                    "bytes" => format!("b\"{}\"", source_bytes_literal(&bytes)),
                    other => bail!("unknown static literal kind {other}"),
                }
            }
            "literal_bool" => payload
                .get("value")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| anyhow!("literal_bool missing value"))?
                .to_string(),
            "literal_unit" => "()".to_string(),
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                local_params
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("p{index}"))
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                local_at_depth(local_names, depth)
                    .cloned()
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))?
            }
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let args = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                let rendered_args = args
                    .iter()
                    .map(|arg| {
                        let hash = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        self.expr_to_source_with_locals(
                            hash,
                            root,
                            current_module,
                            local_params,
                            region_names,
                            local_names,
                            0,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                format!(
                    "{}({})",
                    self.symbol_display_for_module(root, current_module, symbol)?,
                    rendered_args.join(", ")
                )
            }
            "binary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?;
                let prec = op_precedence(op);
                let left = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                let expr = format!(
                    "{} {} {}",
                    self.expr_to_source_with_locals(
                        left,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        prec,
                    )?,
                    op,
                    self.expr_to_source_with_locals(
                        right,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        prec + 1,
                    )?
                );
                if prec < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "unary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?;
                let child = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?;
                let prec = unary_precedence();
                let expr = format!(
                    "{op}{}",
                    self.expr_to_source_with_locals(
                        child,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        prec,
                    )?
                );
                if prec < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "borrow_shared" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing target"))?;
                let region = payload.get("region_name").and_then(JsonValue::as_str);
                let rendered_target = self.expr_to_source_with_locals(
                    target,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    unary_precedence(),
                )?;
                let expr = match region {
                    Some(region) => format!("&'{region} {rendered_target}"),
                    None => format!("&{rendered_target}"),
                };
                if unary_precedence() < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "borrow_mut" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing target"))?;
                let region = payload.get("region_name").and_then(JsonValue::as_str);
                let rendered_target = self.expr_to_source_with_locals(
                    target,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    unary_precedence(),
                )?;
                let expr = match region {
                    Some(region) => format!("&'{region} mut {rendered_target}"),
                    None => format!("&mut {rendered_target}"),
                };
                if unary_precedence() < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "slice_from_array" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_from_array missing target"))?;
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("slice_from_array missing mutable"))?;
                let rendered_target = self.expr_to_source_with_locals(
                    target,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                )?;
                let name = if mutable { "mut_slice" } else { "slice" };
                format!("{name}({rendered_target})")
            }
            "slice_len" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_len missing target"))?;
                format!(
                    "len({})",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "subslice" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing target"))?;
                let start = payload
                    .get("start")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing start"))?;
                let len = payload
                    .get("len")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing len"))?;
                format!(
                    "subslice({}, {}, {})",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        start,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        len,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "box_new" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("box_new missing value"))?;
                format!(
                    "box_new({})",
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "unbox" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unbox missing value"))?;
                format!(
                    "unbox({})",
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "int_cast" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing value"))?;
                let target = payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing type"))?;
                let target_name = crate::types::scalar_int_source_name_for_hash(target)
                    .ok_or_else(|| anyhow!("int_cast target is not a sized integer"))?;
                format!(
                    "to_{target_name}({})",
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "vec_new" => {
                let capacity = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_new missing capacity"))?;
                format!(
                    "vec_new({})",
                    self.expr_to_source_with_locals(
                        capacity,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "vec_push" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_push missing target"))?;
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_push missing value"))?;
                format!(
                    "vec_push({}, {})",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "vec_get" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_get missing target"))?;
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_get missing index"))?;
                format!(
                    "vec_get({}, {})",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        index,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "vec_len" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_len missing target"))?;
                format!(
                    "vec_len({})",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "string_new" => {
                let source = payload
                    .get("source")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_new missing source"))?;
                format!(
                    "string_new({})",
                    self.expr_to_source_with_locals(
                        source,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "string_len" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_len missing target"))?;
                format!(
                    "string_len({})",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "string_with_capacity" => {
                let capacity = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_with_capacity missing capacity"))?;
                format!(
                    "string_with_capacity({})",
                    self.expr_to_source_with_locals(
                        capacity,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "string_push" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_push missing target"))?;
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_push missing value"))?;
                format!(
                    "string_push({}, {})",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "string_get" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_get missing target"))?;
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_get missing index"))?;
                format!(
                    "string_get({}, {})",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        index,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "raw_ptr_cast" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?;
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing mutable"))?;
                let name = if mutable { "raw_mut_ptr" } else { "raw_ptr" };
                format!(
                    "{name}({})",
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "raw_load" => {
                let pointer = payload
                    .get("pointer")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_load missing pointer"))?;
                format!(
                    "raw_load({})",
                    self.expr_to_source_with_locals(
                        pointer,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "raw_store" => {
                let pointer = payload
                    .get("pointer")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_store missing pointer"))?;
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_store missing value"))?;
                format!(
                    "raw_store({}, {})",
                    self.expr_to_source_with_locals(
                        pointer,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                )
            }
            "assign" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("assign missing target"))?;
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("assign missing value"))?;
                let expr = format!(
                    "{} = {}",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        assignment_precedence() + 1,
                    )?,
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        assignment_precedence(),
                    )?
                );
                if assignment_precedence() < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "let" => {
                let name = payload
                    .get("binding_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_name"))?;
                let binding_type = payload
                    .get("binding_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_type"))?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing value"))?;
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing body"))?;
                let value = self.expr_to_source_with_locals(
                    value_hash,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                )?;
                local_names.push(name.to_string());
                let body = self.expr_to_source_with_locals(
                    body_hash,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                );
                local_names.pop();
                let expr = format!(
                    "let {name}: {} = {value} in {}",
                    self.type_name_in_root_with_regions(
                        root,
                        current_module,
                        binding_type,
                        region_names,
                    )?,
                    body?
                );
                if parent_prec > 0 {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "if" => {
                let cond = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing cond"))?;
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                let expr = format!(
                    "if {} then {} else {}",
                    self.expr_to_source_with_locals(
                        cond,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        then_hash,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        else_hash,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                );
                if parent_prec > 0 {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "return" => {
                // `return <value>` (R7). The operand renders at precedence 0 — a
                // `return` greedily takes the whole following expression, so its
                // operand never needs parens relative to `return` — and the whole
                // form parenthesizes when nested in a higher-precedence position.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("return missing value"))?;
                let expr = format!(
                    "return {}",
                    self.expr_to_source_with_locals(
                        value,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                );
                if parent_prec > 0 {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "fold" => {
                let item_name = payload
                    .get("item_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing item_name"))?;
                let acc_name = payload
                    .get("acc_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing acc_name"))?;
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing target"))?;
                let init_hash = payload
                    .get("init")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing init"))?;
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing body"))?;
                let target = self.expr_to_source_with_locals(
                    target_hash,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                )?;
                let init = self.expr_to_source_with_locals(
                    init_hash,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                )?;
                local_names.push(item_name.to_string());
                local_names.push(acc_name.to_string());
                let body = self.expr_to_source_with_locals(
                    body_hash,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                );
                local_names.pop();
                local_names.pop();
                let expr = format!(
                    "fold {item_name} in {target} with {acc_name} = {init} do {}",
                    body?
                );
                if parent_prec > 0 {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "loop" => {
                let acc_name = payload
                    .get("acc_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing acc_name"))?;
                let init_hash = payload
                    .get("init")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing init"))?;
                let cond_hash = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing cond"))?;
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing body"))?;
                // `init` renders without `acc` in scope; `cond` and `body` with it.
                let init = self.expr_to_source_with_locals(
                    init_hash,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                )?;
                local_names.push(acc_name.to_string());
                let cond = self.expr_to_source_with_locals(
                    cond_hash,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                );
                let body = self.expr_to_source_with_locals(
                    body_hash,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                );
                local_names.pop();
                let expr = format!(
                    "loop {acc_name} = {init} while {} do {}",
                    cond?, body?
                );
                if parent_prec > 0 {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "record_literal" => {
                let fields = payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                    .iter()
                    .map(|field| {
                        let name = field
                            .get("name")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing name"))?;
                        let value = field
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing value"))?;
                        Ok(format!(
                            "{name}: {}",
                            self.expr_to_source_with_locals(
                                value,
                                root,
                                current_module,
                                local_params,
                                region_names,
                                local_names,
                                0,
                            )?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                format!("{{{}}}", fields.join(", "))
            }
            "array_literal" => {
                let elements = payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                    .iter()
                    .map(|element| {
                        let value = element
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array element missing value"))?;
                        self.expr_to_source_with_locals(
                            value,
                            root,
                            current_module,
                            local_params,
                            region_names,
                            local_names,
                            0,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                format!("[{}]", elements.join(", "))
            }
            "array_fill" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                let count = payload
                    .get("count")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("array_fill missing count"))?;
                let value = self.expr_to_source_with_locals(
                    value,
                    root,
                    current_module,
                    local_params,
                    region_names,
                    local_names,
                    0,
                )?;
                format!("[{value}; {count}]")
            }
            "array_set" => {
                // Projects as the builtin call `array_set(arr, i, v)`, re-parsed as a
                // normal call and re-dispatched to the builtin on re-import.
                let mut rendered = Vec::with_capacity(3);
                for key in ["array", "index", "value"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array_set missing {key}"))?;
                    rendered.push(self.expr_to_source_with_locals(
                        child,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?);
                }
                format!("array_set({})", rendered.join(", "))
            }
            "array_index" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing index"))?;
                let expr = format!(
                    "{}[{}]",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        field_access_precedence(),
                    )?,
                    self.expr_to_source_with_locals(
                        index,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?
                );
                if field_access_precedence() < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "field_access" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                let field = payload
                    .get("field")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing field"))?;
                let expr = format!(
                    "{}.{field}",
                    self.expr_to_source_with_locals(
                        target,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        field_access_precedence(),
                    )?
                );
                if field_access_precedence() < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "enum_construct" => {
                let enum_type = payload
                    .get("enum_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing enum_type"))?;
                let variant = payload
                    .get("variant")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing variant"))?;
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                if payload.get("value").is_some()
                    && payload.get("value").and_then(JsonValue::as_str).is_some()
                    && self
                        .get_payload(value)?
                        .get("expr_kind")
                        .and_then(JsonValue::as_str)
                        == Some("literal_unit")
                {
                    format!(
                        "{}::{variant}",
                        self.enum_constructor_type_source(
                            root,
                            current_module,
                            enum_type,
                            region_names,
                        )?
                    )
                } else {
                    format!(
                        "{}::{variant}({})",
                        self.enum_constructor_type_source(
                            root,
                            current_module,
                            enum_type,
                            region_names,
                        )?,
                        self.expr_to_source_with_locals(
                            value,
                            root,
                            current_module,
                            local_params,
                            region_names,
                            local_names,
                            0,
                        )?
                    )
                }
            }
            "case" => {
                let expr_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?;
                let arm_count = arms.len();
                let rendered_arms = arms
                    .iter()
                    .map(|arm| {
                        // A nested low-precedence body must be parenthesized so the
                        // OUTER case's `| arm` list (or a bitwise `|` in the body)
                        // re-parses correctly (SPEC_V3 §11 checked-view round-trip).
                        // Rendering at the case-arm-body floor parenthesizes both a
                        // nested `case`/`if`/`let`/`fold` and a top-level bitwise `|`
                        // (whose precedence sits just below the floor): the arm-body
                        // parser ends at a top-level `|`, so it must be parenthesized
                        // in every arm, including the last.
                        let _ = arm_count;
                        let body_prec = crate::op_registry::CASE_ARM_BODY_MIN_PRECEDENCE;
                        let binding = arm.get("binding_name").and_then(JsonValue::as_str);
                        // An `if <guard>` (R14) renders between the pattern and `=>`.
                        // Scalar arms bind nothing, so the guard renders in the current
                        // local scope; the variant arm renders its own guard inside the
                        // binding scope below. Prec 1 parenthesizes a compound guard so
                        // a trailing `=>`/`|` cannot be captured on re-parse.
                        let scalar_guard_suffix = match arm.get("guard").and_then(JsonValue::as_str)
                        {
                            Some(guard) => format!(
                                " if {}",
                                self.expr_to_source_with_locals(
                                    guard,
                                    root,
                                    current_module,
                                    local_params,
                                    region_names,
                                    local_names,
                                    1,
                                )?
                            ),
                            None => String::new(),
                        };
                        if typed_case_arm_is_default(arm) {
                            if binding.is_some() {
                                bail!("default case arm cannot bind a payload");
                            }
                            let body = arm
                                .get("body")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("case arm missing body"))?;
                            let rendered_body = self.expr_to_source_with_locals(
                                body,
                                root,
                                current_module,
                                local_params,
                                region_names,
                                local_names,
                                body_prec,
                            )?;
                            // A guarded wildcard re-parses as a non-terminal `_` arm;
                            // the unguarded catch-all renders as `else`.
                            if scalar_guard_suffix.is_empty() {
                                return Ok(format!("else => {rendered_body}"));
                            }
                            return Ok(format!("_{scalar_guard_suffix} => {rendered_body}"));
                        }
                        // Scalar literal pattern (R14): `0 => ...`, `true => ...`.
                        if let Some(literal) = scalar_literal_pattern_from_typed_arm(arm) {
                            if binding.is_some() {
                                bail!("scalar case arm cannot bind a value");
                            }
                            let pattern = match literal.as_ref() {
                                RawExpr::LiteralI64 { value } => value.clone(),
                                RawExpr::LiteralBool { value } => value.to_string(),
                                _ => bail!("invalid scalar case literal pattern"),
                            };
                            let body = arm
                                .get("body")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("case arm missing body"))?;
                            return Ok(format!(
                                "{pattern}{scalar_guard_suffix} => {}",
                                self.expr_to_source_with_locals(
                                    body,
                                    root,
                                    current_module,
                                    local_params,
                                    region_names,
                                    local_names,
                                    body_prec,
                                )?
                            ));
                        }
                        // Scalar range pattern (R14): `lo..hi` / `lo..=hi`.
                        if let Some(range) = scalar_range_pattern_from_typed_arm(arm) {
                            if binding.is_some() {
                                bail!("scalar case arm cannot bind a value");
                            }
                            let bound = |expr: &RawExpr| match expr {
                                RawExpr::LiteralI64 { value } => Ok(value.clone()),
                                _ => bail!("invalid range case bound"),
                            };
                            let lo = bound(range.lo.as_ref())?;
                            let hi = bound(range.hi.as_ref())?;
                            let dots = if range.inclusive { "..=" } else { ".." };
                            let body = arm
                                .get("body")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("case arm missing body"))?;
                            return Ok(format!(
                                "{lo}{dots}{hi}{scalar_guard_suffix} => {}",
                                self.expr_to_source_with_locals(
                                    body,
                                    root,
                                    current_module,
                                    local_params,
                                    region_names,
                                    local_names,
                                    body_prec,
                                )?
                            ));
                        }
                        let variant = arm
                            .get("variant")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing variant"))?;
                        let body = arm
                            .get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?;
                        let guard_hash = arm.get("guard").and_then(JsonValue::as_str);
                        // A nested destructuring pattern (R14) renders as
                        // `outer(inner(..))`; its leaf binding (like a simple binding)
                        // is in scope for the guard and body.
                        let payload_pattern = nested_pattern_from_typed_arm(arm);
                        let scoped_binding = binding.map(str::to_string).or_else(|| {
                            payload_pattern
                                .as_ref()
                                .and_then(pattern_leaf_binding)
                                .map(str::to_string)
                        });
                        if let Some(name) = &scoped_binding {
                            local_names.push(name.clone());
                        }
                        // The guard (if any) renders inside the binding scope so it can
                        // reference the bound payload; the body follows.
                        let rendered_guard = match guard_hash {
                            Some(guard) => Some(self.expr_to_source_with_locals(
                                guard,
                                root,
                                current_module,
                                local_params,
                                region_names,
                                local_names,
                                1,
                            )?),
                            None => None,
                        };
                        let rendered_body = self.expr_to_source_with_locals(
                            body,
                            root,
                            current_module,
                            local_params,
                            region_names,
                            local_names,
                            body_prec,
                        );
                        if scoped_binding.is_some() {
                            local_names.pop();
                        }
                        let guard_suffix = match rendered_guard {
                            Some(guard) => format!(" if {guard}"),
                            None => String::new(),
                        };
                        let pattern_text = if let Some(pattern) = &payload_pattern {
                            format!("{variant}({})", render_pattern(pattern))
                        } else if let Some(binding) = binding {
                            format!("{variant}({binding})")
                        } else {
                            variant.to_string()
                        };
                        Ok(format!("{pattern_text}{guard_suffix} => {}", rendered_body?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let expr = format!(
                    "case {} of {}",
                    self.expr_to_source_with_locals(
                        expr_hash,
                        root,
                        current_module,
                        local_params,
                        region_names,
                        local_names,
                        0,
                    )?,
                    rendered_arms.join(" | ")
                );
                if parent_prec > 0 {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            other => bail!("unknown expression kind {other}"),
        };
        Ok(rendered)
    }

    pub(crate) fn typed_expr_to_raw(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
    ) -> Result<RawExpr> {
        self.typed_expr_to_raw_in_module(expr_hash, root, MAIN_BRANCH)
    }

    pub(crate) fn typed_expr_to_raw_in_module(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
    ) -> Result<RawExpr> {
        self.typed_expr_to_raw_in_module_with_regions(
            expr_hash,
            root,
            current_module,
            &BTreeMap::new(),
        )
    }

    pub(crate) fn typed_expr_to_raw_in_module_with_regions(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        region_names: &BTreeMap<String, String>,
    ) -> Result<RawExpr> {
        self.typed_expr_to_raw_with_locals(
            expr_hash,
            root,
            current_module,
            region_names,
            &mut Vec::new(),
            None,
        )
    }

    pub(crate) fn typed_expr_to_raw_in_module_with_regions_and_locals(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        region_names: &BTreeMap<String, String>,
        local_names: &[String],
    ) -> Result<RawExpr> {
        let mut local_names = local_names.to_vec();
        self.typed_expr_to_raw_with_locals(
            expr_hash,
            root,
            current_module,
            region_names,
            &mut local_names,
            None,
        )
    }

    /// The complete typed→raw converter with a patch hook active — see
    /// [`RawConversionHook`]. `local_names` seeds the let-binding scope (the
    /// function's parameter names are NOT locals; depth-indexed `local_ref`s
    /// resolve against this stack).
    pub(crate) fn typed_expr_to_raw_hooked(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        region_names: &BTreeMap<String, String>,
        local_names: &mut Vec<String>,
        hook: RawConversionHook<'_>,
    ) -> Result<RawExpr> {
        self.typed_expr_to_raw_with_locals(
            expr_hash,
            root,
            current_module,
            region_names,
            local_names,
            Some(hook),
        )
    }

    fn typed_expr_to_raw_with_locals(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        region_names: &BTreeMap<String, String>,
        local_names: &mut Vec<String>,
        hook: Option<RawConversionHook<'_>>,
    ) -> Result<RawExpr> {
        let payload = self.get_payload(expr_hash)?;
        if let Some(active) = hook {
            match active(expr_hash, &payload)? {
                RawHookOutcome::Replace(raw) => return Ok(raw),
                RawHookOutcome::RenameCall(name) => {
                    if payload.get("expr_kind").and_then(JsonValue::as_str) != Some("call") {
                        bail!("call replacement matched non-call expression {expr_hash}");
                    }
                    let args = payload
                        .get("args")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("call missing args"))?
                        .iter()
                        .map(|arg| {
                            let hash = arg
                                .as_str()
                                .ok_or_else(|| anyhow!("call arg must be hash"))?;
                            self.typed_expr_to_raw_with_locals(
                                hash,
                                root,
                                current_module,
                                region_names,
                                local_names,
                                hook,
                            )
                        })
                        .collect::<Result<Vec<_>>>()?;
                    return Ok(RawExpr::Call { name, args });
                }
                RawHookOutcome::InlineCall => {
                    if payload.get("expr_kind").and_then(JsonValue::as_str) != Some("call") {
                        bail!("inline_function matched non-call expression {expr_hash}");
                    }
                    return self.inline_call_payload(&payload, root, local_names);
                }
                RawHookOutcome::Continue => {}
            }
        }
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .to_string();
                // A NEGATIVE literal node exists only via the typing-side fold
                // of `-MIN_DIGITS` (#9 — the positive half is unrepresentable
                // at the width). Its source form is the unary minus the user
                // wrote, so render it back as one: the raw view never sees
                // folded literals and `function_source_matches`/projection
                // round-trips stay exact.
                if let Some(digits) = value.strip_prefix('-') {
                    return Ok(RawExpr::Unary {
                        op: "-".to_string(),
                        expr: Box::new(RawExpr::LiteralI64 {
                            value: digits.to_string(),
                        }),
                    });
                }
                Ok(RawExpr::LiteralI64 { value })
            }
            "literal_bool" => Ok(RawExpr::LiteralBool {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?,
            }),
            "static_bytes" => {
                let bytes_hex = self.static_data_bytes_hex(
                    payload
                        .get("static_data")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("static_bytes missing static_data"))?,
                )?;
                match payload
                    .get("literal_kind")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing literal_kind"))?
                {
                    "string" => Ok(RawExpr::LiteralString {
                        value: String::from_utf8(hex_to_bytes(&bytes_hex)?)
                            .map_err(|_| anyhow!("string literal static data is not utf8"))?,
                    }),
                    "bytes" => Ok(RawExpr::LiteralBytes { bytes_hex }),
                    other => bail!("unknown static literal kind {other}"),
                }
            }
            "literal_unit" => Ok(RawExpr::Unit),
            "param_ref" => Ok(RawExpr::ParamRef {
                index: payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize,
            }),
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                Ok(RawExpr::ParamName {
                    name: local_at_depth(local_names, depth)
                        .cloned()
                        .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))?,
                })
            }
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let args = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                Ok(RawExpr::Call {
                    name: self.symbol_display_for_module(root, current_module, symbol)?,
                    args: args
                        .iter()
                        .map(|arg| {
                            let hash = arg
                                .as_str()
                                .ok_or_else(|| anyhow!("call arg must be hash"))?;
                            self.typed_expr_to_raw_with_locals(
                                hash,
                                root,
                                current_module,
                                region_names,
                                local_names,
                                hook,
                            )
                        })
                        .collect::<Result<Vec<_>>>()?,
                })
            }
            "binary" => Ok(RawExpr::Binary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?
                    .to_string(),
                left: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("left")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing left"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
                right: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("right")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing right"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "unary" => Ok(RawExpr::Unary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?
                    .to_string(),
                expr: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("expr")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("unary missing expr"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "borrow_shared" => Ok(RawExpr::BorrowShared {
                region: payload
                    .get("region_name")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
                target: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("borrow_shared missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "borrow_mut" => Ok(RawExpr::BorrowMut {
                region: payload
                    .get("region_name")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
                target: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("borrow_mut missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "slice_from_array" => {
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("slice_from_array missing mutable"))?;
                Ok(RawExpr::Call {
                    name: if mutable {
                        "mut_slice".to_string()
                    } else {
                        "slice".to_string()
                    },
                    args: vec![
                        self.typed_expr_to_raw_with_locals(
                            payload
                                .get("target")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("slice_from_array missing target"))?,
                            root,
                            current_module,
                            region_names,
                            local_names,
                            hook,
                        )?,
                    ],
                })
            }
            "slice_len" => Ok(RawExpr::Call {
                name: "len".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("slice_len missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "subslice" => Ok(RawExpr::Call {
                name: "subslice".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("subslice missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("start")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("subslice missing start"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("len")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("subslice missing len"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "box_new" => Ok(RawExpr::Call {
                name: "box_new".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("box_new missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "unbox" => Ok(RawExpr::Call {
                name: "unbox".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("unbox missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "int_cast" => {
                let target = payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing type"))?;
                let target_name = crate::types::scalar_int_source_name_for_hash(target)
                    .ok_or_else(|| anyhow!("int_cast target is not a sized integer"))?;
                Ok(RawExpr::Call {
                    name: format!("to_{target_name}"),
                    args: vec![self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("int_cast missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?],
                })
            }
            "vec_new" => Ok(RawExpr::Call {
                name: "vec_new".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("capacity")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("vec_new missing capacity"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "vec_push" => Ok(RawExpr::Call {
                name: "vec_push".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("vec_push missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("vec_push missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "vec_get" => Ok(RawExpr::Call {
                name: "vec_get".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("vec_get missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("index")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("vec_get missing index"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "vec_len" => Ok(RawExpr::Call {
                name: "vec_len".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("vec_len missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "string_new" => Ok(RawExpr::Call {
                name: "string_new".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("source")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("string_new missing source"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "string_len" => Ok(RawExpr::Call {
                name: "string_len".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("string_len missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "string_with_capacity" => Ok(RawExpr::Call {
                name: "string_with_capacity".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("capacity")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("string_with_capacity missing capacity"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "string_push" => Ok(RawExpr::Call {
                name: "string_push".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("string_push missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("string_push missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "string_get" => Ok(RawExpr::Call {
                name: "string_get".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("string_get missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("index")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("string_get missing index"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "raw_ptr_cast" => {
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing mutable"))?;
                Ok(RawExpr::Call {
                    name: if mutable {
                        "raw_mut_ptr".to_string()
                    } else {
                        "raw_ptr".to_string()
                    },
                    args: vec![
                        self.typed_expr_to_raw_with_locals(
                            payload
                                .get("value")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?,
                            root,
                            current_module,
                            region_names,
                            local_names,
                            hook,
                        )?,
                    ],
                })
            }
            "raw_load" => Ok(RawExpr::Call {
                name: "raw_load".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("pointer")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("raw_load missing pointer"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "raw_store" => Ok(RawExpr::Call {
                name: "raw_store".to_string(),
                args: vec![
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("pointer")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("raw_store missing pointer"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("raw_store missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ],
            }),
            "assign" => Ok(RawExpr::Assign {
                target: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("assign missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
                value: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("assign missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "let" => {
                let name = payload
                    .get("binding_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_name"))?
                    .to_string();
                let binding_type = payload
                    .get("binding_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_type"))?;
                let value = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing value"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                )?;
                local_names.push(name.clone());
                let body = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing body"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                );
                local_names.pop();
                Ok(RawExpr::Let {
                    name,
                    ty: self
                        .type_name_in_root_with_regions(
                            root,
                            current_module,
                            binding_type,
                            region_names,
                        )?
                        .to_string(),
                    value: Box::new(value),
                    body: Box::new(body?),
                })
            }
            "if" => Ok(RawExpr::If {
                cond: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("cond")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing cond"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
                then_expr: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("then")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing then"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
                else_expr: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("else")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing else"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "return" => Ok(RawExpr::Return {
                value: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("return missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "fold" => {
                let item = payload
                    .get("item_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing item_name"))?
                    .to_string();
                let acc = payload
                    .get("acc_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing acc_name"))?
                    .to_string();
                let target = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("target")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("fold missing target"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                )?;
                let init = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("init")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("fold missing init"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                )?;
                local_names.push(item.clone());
                local_names.push(acc.clone());
                let body = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("fold missing body"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                );
                local_names.pop();
                local_names.pop();
                Ok(RawExpr::Fold {
                    item,
                    target: Box::new(target),
                    acc,
                    init: Box::new(init),
                    body: Box::new(body?),
                })
            }
            "loop" => {
                let acc = payload
                    .get("acc_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing acc_name"))?
                    .to_string();
                let init = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("init")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("loop missing init"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                )?;
                local_names.push(acc.clone());
                let cond = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("cond")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("loop missing cond"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                );
                let body = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("loop missing body"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                );
                local_names.pop();
                Ok(RawExpr::Loop {
                    acc,
                    init: Box::new(init),
                    cond: Box::new(cond?),
                    body: Box::new(body?),
                })
            }
            "record_literal" => Ok(RawExpr::Record {
                fields: payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                    .iter()
                    .map(|field| {
                        let name = field
                            .get("name")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing name"))?
                            .to_string();
                        let value = self.typed_expr_to_raw_with_locals(
                            field
                                .get("value")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("record field missing value"))?,
                            root,
                            current_module,
                            region_names,
                            local_names,
                            hook,
                        )?;
                        Ok(RawRecordField { name, value })
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
            "array_literal" => Ok(RawExpr::Array {
                elements: payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                    .iter()
                    .map(|element| {
                        self.typed_expr_to_raw_with_locals(
                            element
                                .get("value")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("array element missing value"))?,
                            root,
                            current_module,
                            region_names,
                            local_names,
                            hook,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
            "array_fill" => Ok(RawExpr::ArrayFill {
                value: Box::new(self.typed_expr_to_raw_with_locals(
                    payload
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array_fill missing value"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                )?),
                count: payload
                    .get("count")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("array_fill missing count"))?
                    .to_string(),
            }),
            "array_set" => {
                // Reconstructs to the builtin call `array_set(arr, i, v)`, re-typed
                // back to this node by the type checker.
                let mut call_args = Vec::with_capacity(3);
                for key in ["array", "index", "value"] {
                    call_args.push(self.typed_expr_to_raw_with_locals(
                        payload
                            .get(key)
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array_set missing {key}"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?);
                }
                Ok(RawExpr::Call {
                    name: "array_set".to_string(),
                    args: call_args,
                })
            }
            "array_index" => Ok(RawExpr::Index {
                target: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array_index missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
                index: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("index")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array_index missing index"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "field_access" => Ok(RawExpr::FieldAccess {
                target: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("target")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("field_access missing target"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
                field: payload
                    .get("field")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing field"))?
                    .to_string(),
            }),
            "enum_construct" => Ok(RawExpr::EnumConstruct {
                enum_type: self.enum_constructor_type_source(
                    root,
                    current_module,
                    payload
                        .get("enum_type")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("enum_construct missing enum_type"))?,
                    region_names,
                )?,
                variant: payload
                    .get("variant")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing variant"))?
                    .to_string(),
                value: Box::new(
                    self.typed_expr_to_raw_with_locals(
                        payload
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("enum_construct missing value"))?,
                        root,
                        current_module,
                        region_names,
                        local_names,
                        hook,
                    )?,
                ),
            }),
            "case" => {
                let expr = self.typed_expr_to_raw_with_locals(
                    payload
                        .get("expr")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case missing expr"))?,
                    root,
                    current_module,
                    region_names,
                    local_names,
                    hook,
                )?;
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?
                    .iter()
                    .map(|arm| {
                        let binding = arm
                            .get("binding_name")
                            .and_then(JsonValue::as_str)
                            .map(str::to_string);
                        // A guard (R14) on a scalar/wildcard arm reconstructs in the
                        // current (no-binding) scope, exactly like the body.
                        let scalar_guard = match arm.get("guard").and_then(JsonValue::as_str) {
                            Some(guard) => Some(Box::new(self.typed_expr_to_raw_with_locals(
                                guard,
                                root,
                                current_module,
                                region_names,
                                local_names,
                                hook,
                            )?)),
                            None => None,
                        };
                        if typed_case_arm_is_default(arm) {
                            if binding.is_some() {
                                bail!("default case arm cannot bind a payload");
                            }
                            let body = self.typed_expr_to_raw_with_locals(
                                arm.get("body")
                                    .and_then(JsonValue::as_str)
                                    .ok_or_else(|| anyhow!("case arm missing body"))?,
                                root,
                                current_module,
                                region_names,
                                local_names,
                                hook,
                            )?;
                            return Ok(RawCaseArm {
                                variant: None,
                                literal: None,
                                range: None,
                                default: true,
                                binding: None,
                                guard: scalar_guard,
                                payload_pattern: None,
                                body,
                            });
                        }
                        // Scalar literal pattern (R14): no variant, no binding.
                        if let Some(literal) = scalar_literal_pattern_from_typed_arm(arm) {
                            let body = self.typed_expr_to_raw_with_locals(
                                arm.get("body")
                                    .and_then(JsonValue::as_str)
                                    .ok_or_else(|| anyhow!("case arm missing body"))?,
                                root,
                                current_module,
                                region_names,
                                local_names,
                                hook,
                            )?;
                            return Ok(RawCaseArm {
                                variant: None,
                                literal: Some(literal),
                                range: None,
                                default: false,
                                binding: None,
                                guard: scalar_guard,
                                payload_pattern: None,
                                body,
                            });
                        }
                        // Scalar range pattern (R14): no variant, no binding.
                        if let Some(range) = scalar_range_pattern_from_typed_arm(arm) {
                            let body = self.typed_expr_to_raw_with_locals(
                                arm.get("body")
                                    .and_then(JsonValue::as_str)
                                    .ok_or_else(|| anyhow!("case arm missing body"))?,
                                root,
                                current_module,
                                region_names,
                                local_names,
                                hook,
                            )?;
                            return Ok(RawCaseArm {
                                variant: None,
                                literal: None,
                                range: Some(range),
                                default: false,
                                binding: None,
                                guard: scalar_guard,
                                payload_pattern: None,
                                body,
                            });
                        }
                        let variant = arm
                            .get("variant")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing variant"))?
                            .to_string();
                        // Reconstruct a nested destructuring pattern (R14); its leaf
                        // binding (like a simple binding) is in scope for the guard/body.
                        let payload_pattern = nested_pattern_from_typed_arm(arm);
                        let scoped_binding = binding.clone().or_else(|| {
                            payload_pattern
                                .as_ref()
                                .and_then(pattern_leaf_binding)
                                .map(str::to_string)
                        });
                        if let Some(name) = &scoped_binding {
                            local_names.push(name.clone());
                        }
                        // The guard (if any) reconstructs inside the binding scope, like
                        // the body (`.map` is eager, so it runs before the pop below);
                        // both `?`-propagate only after the binding is popped so
                        // `local_names` stays balanced.
                        let guard = arm.get("guard").and_then(JsonValue::as_str).map(|guard| {
                            self.typed_expr_to_raw_with_locals(
                                guard,
                                root,
                                current_module,
                                region_names,
                                local_names,
                                hook,
                            )
                        });
                        let body = self.typed_expr_to_raw_with_locals(
                            arm.get("body")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("case arm missing body"))?,
                            root,
                            current_module,
                            region_names,
                            local_names,
                            hook,
                        );
                        if scoped_binding.is_some() {
                            local_names.pop();
                        }
                        let guard = guard.transpose()?.map(Box::new);
                        Ok(RawCaseArm {
                            variant: Some(variant),
                            literal: None,
                            range: None,
                            default: false,
                            binding,
                            guard,
                            payload_pattern,
                            body: body?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(RawExpr::Case {
                    expr: Box::new(expr),
                    arms,
                })
            }
            other => bail!("unknown expression kind {other}"),
        }
    }
}

pub(crate) fn eval_binary(op: &str, left: Value, right: Value) -> Result<Value> {
    crate::op_registry::eval_binary(op, left, right)
}

pub(crate) fn eval_unary(op: &str, value: Value) -> Result<Value> {
    crate::op_registry::eval_unary(op, value)
}

pub(crate) fn op_precedence(op: &str) -> u8 {
    crate::op_registry::binary_precedence(op)
}

pub(crate) fn unary_precedence() -> u8 {
    crate::op_registry::UNARY_PRECEDENCE
}

fn assignment_precedence() -> u8 {
    1
}

fn field_access_precedence() -> u8 {
    // Postfix field access / indexing binds tightest — above unary
    // (`op_registry::UNARY_PRECEDENCE`), so `-x.f` projects without parenthesizing
    // `x.f`. Kept above the rescaled operator precedences (Phase 9 widened the
    // binary precedence scale; this must stay the maximum).
    80
}

fn signature_region_name_map(params: &[RegionParamDef]) -> BTreeMap<String, String> {
    params
        .iter()
        .map(|param| (param.region.clone(), param.name.clone()))
        .collect()
}

fn signature_region_suffix(params: &[RegionParamDef]) -> String {
    if params.is_empty() {
        String::new()
    } else {
        format!(
            "<{}>",
            params
                .iter()
                .map(|param| format!("'{}", param.name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Render a function header's `<'r, T>` parameter list (R11): region parameters
/// first (each `'name`), then type parameters (each a bare name) — the order
/// `parse_optional_region_and_type_params` accepts, so the projection re-parses
/// to the same signature. Empty when the function has neither.
fn signature_region_and_type_suffix(
    region_params: &[RegionParamDef],
    type_param_names: &[String],
) -> String {
    if region_params.is_empty() && type_param_names.is_empty() {
        return String::new();
    }
    let parts = region_params
        .iter()
        .map(|param| format!("'{}", param.name))
        .chain(type_param_names.iter().cloned())
        .collect::<Vec<_>>();
    format!("<{}>", parts.join(", "))
}

fn field_access_from_path(path: &str) -> RawExpr {
    let mut parts = path.split('.');
    let first = parts.next().unwrap_or_default().to_string();
    let mut expr = RawExpr::ParamName { name: first };
    for field in parts {
        expr = RawExpr::FieldAccess {
            target: Box::new(expr),
            field: field.to_string(),
        };
    }
    expr
}

fn source_string_literal(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}

fn source_bytes_literal(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        match *byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            0 => out.push_str("\\0"),
            0x20..=0x7e => out.push(*byte as char),
            other => out.push_str(&format!("\\x{other:02x}")),
        }
    }
    out
}

impl CodeDb {
    pub(crate) fn dependencies_for_definition(
        &self,
        root: &ProgramRootPayload,
        definition_hash: &str,
    ) -> Result<BTreeSet<String>> {
        if self.definition_is_external(definition_hash)? {
            return Ok(BTreeSet::new());
        }
        let body = self.function_body_hash(definition_hash)?;
        let mut deps = BTreeSet::new();
        self.collect_expr_deps(root, &body, &mut deps)?;
        Ok(deps)
    }

    /// The *named* function dependencies of a definition (R11): the named callee
    /// each call site mentions (`collect_named_call_symbols`) — the generic itself
    /// for a generic call. Build reachability (`dependencies_for_definition`) follows
    /// the unnamed monomorphic instances instead; source-level concerns — projection
    /// ordering — follow the named generic, so the projection emits a callee before
    /// its caller and re-imports to the same program. Reading the named callee
    /// directly (rather than mapping a build-dep instance back to its generic) also
    /// keeps a generic recursion group's in-clique calls — at `TypeParam` arguments,
    /// whose instance does not exist — from being dropped.
    pub(crate) fn named_dependencies_for_definition(
        &self,
        _root: &ProgramRootPayload,
        definition_hash: &str,
    ) -> Result<BTreeSet<String>> {
        if self.definition_is_external(definition_hash)? {
            return Ok(BTreeSet::new());
        }
        let body = self.function_body_hash(definition_hash)?;
        let mut named = BTreeSet::new();
        self.collect_named_call_symbols(&body, &mut named)?;
        Ok(named)
    }

    /// Collect the NAMED callee symbol of every call in a typed body: the symbol a
    /// call site mentions — a generic function's own (named) symbol for a generic
    /// call, the function itself otherwise. Unlike `collect_expr_deps` (build
    /// reachability), this never routes a generic call through its unnamed
    /// monomorphic instance, so a call whose type arguments are not concrete — a
    /// recursive or mutually-recursive generic clique calls itself/its peers at
    /// `TypeParam` arguments, whose instance does not exist — is NOT dropped. That
    /// keeps the projection's topological order (callee before caller) correct for a
    /// generic recursion group, so a non-clique function projected alongside it keeps
    /// its parse position and the import→export→import root hash is a fixpoint (R11).
    fn collect_named_call_symbols(
        &self,
        expr_hash: &str,
        out: &mut BTreeSet<String>,
    ) -> Result<()> {
        let payload = self.get_payload(expr_hash)?;
        if payload.get("expr_kind").and_then(JsonValue::as_str) == Some("call")
            && let Some(symbol) = payload.get("symbol").and_then(JsonValue::as_str)
        {
            out.insert(symbol.to_string());
        }
        for child in self.child_expr_hashes(&payload)? {
            self.collect_named_call_symbols(&child, out)?;
        }
        Ok(())
    }

    pub(crate) fn dependencies_for_type_definition(
        &self,
        _root: &ProgramRootPayload,
        type_def_hash: &str,
    ) -> Result<BTreeSet<String>> {
        let definition = self.type_definition(type_def_hash)?;
        let mut deps = BTreeSet::new();
        match definition {
            TypeDefinition::Record { fields, .. } => {
                for field in fields {
                    self.collect_type_deps(&field.type_hash, &mut deps)?;
                }
            }
            TypeDefinition::Enum { variants, .. } => {
                for variant in variants {
                    self.collect_type_deps(&variant.type_hash, &mut deps)?;
                }
            }
        }
        Ok(deps)
    }

    fn collect_type_deps(&self, type_hash: &str, deps: &mut BTreeSet<String>) -> Result<()> {
        match self.type_spec(type_hash)? {
            TypeSpec::Builtin(_) => {}
            // A type parameter resolves to no concrete type symbol (R11).
            TypeSpec::TypeParam { .. } => {}
            TypeSpec::Named {
                type_symbol,
                type_args,
                ..
            } => {
                deps.insert(type_symbol);
                // A generic instance also depends on the types in its arguments.
                for arg in type_args {
                    self.collect_type_deps(&arg, deps)?;
                }
            }
            TypeSpec::Reference { referent, .. } => {
                self.collect_type_deps(&referent, deps)?;
            }
            TypeSpec::RawPointer { pointee, .. } => {
                self.collect_type_deps(&pointee, deps)?;
            }
            TypeSpec::Box { element } => {
                self.collect_type_deps(&element, deps)?;
            }
            TypeSpec::Vec { element } => {
                self.collect_type_deps(&element, deps)?;
            }
            TypeSpec::String => {}
            TypeSpec::Slice { element, .. } => {
                self.collect_type_deps(&element, deps)?;
            }
            TypeSpec::FixedArray { element, .. } => {
                self.collect_type_deps(&element, deps)?;
            }
            TypeSpec::Record(fields) | TypeSpec::Enum(fields) => {
                for field in fields {
                    self.collect_type_deps(&field.type_hash, deps)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn collect_expr_deps(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        deps: &mut BTreeSet<String>,
    ) -> Result<()> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" | "param_ref"
            | "local_ref" => {}
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                // Generic call (R11): the real build/link dependency is the
                // monomorphic instance derived from the callee and the concrete
                // type arguments, not the generic template (which is never
                // lowered). A non-generic call depends on its callee directly.
                let type_args = crate::types::call_type_args(&payload)?;
                let target = if type_args.is_empty() {
                    symbol.to_string()
                } else {
                    crate::types::monomorphic_instance_symbol(symbol, &type_args)
                };
                if self.root_symbol(root, &target).is_some() {
                    deps.insert(target);
                }
                for arg in payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                {
                    let hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    self.collect_expr_deps(root, hash, deps)?;
                }
            }
            "binary" => {
                let left = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                self.collect_expr_deps(root, left, deps)?;
                self.collect_expr_deps(root, right, deps)?;
            }
            "unary" => {
                let child = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "int_cast" => {
                let child = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing value"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "borrow_shared" | "borrow_mut" => {
                let child = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow expression missing target"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "slice_from_array" | "slice_len" => {
                let child = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice expression missing target"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "subslice" => {
                for key in ["target", "start", "len"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("subslice missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "box_new" => {
                let child = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("box_new missing value"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "unbox" => {
                let child = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unbox missing value"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "vec_new" => {
                let child = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_new missing capacity"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "vec_push" => {
                for key in ["target", "value"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("vec_push missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "vec_get" => {
                for key in ["target", "index"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("vec_get missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "vec_len" => {
                let child = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_len missing target"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "string_new" => {
                let child = payload
                    .get("source")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_new missing source"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "string_len" => {
                let child = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_len missing target"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "string_with_capacity" => {
                let child = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_with_capacity missing capacity"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "string_push" => {
                for key in ["target", "value"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("string_push missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "string_get" => {
                for key in ["target", "index"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("string_get missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "raw_ptr_cast" => {
                let child = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "raw_load" => {
                let child = payload
                    .get("pointer")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_load missing pointer"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "raw_store" => {
                for key in ["pointer", "value"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("raw_store missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "assign" => {
                for key in ["target", "value"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("assign missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "let" => {
                for key in ["value", "body"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "if" => {
                for key in ["cond", "then", "else"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("if missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "return" => {
                let child = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("return missing value"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "fold" => {
                for key in ["target", "init", "body"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("fold missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "loop" => {
                for key in ["init", "cond", "body"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("loop missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "record_literal" => {
                for field in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                {
                    let child = field
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing value"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "array_literal" => {
                for element in payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                {
                    let child = element
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing value"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "array_fill" => {
                let child = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "array_set" => {
                for key in ["array", "index", "value"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array_set missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "array_index" => {
                for key in ["target", "index"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array_index missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            "field_access" => {
                let child = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "enum_construct" => {
                let child = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                self.collect_expr_deps(root, child, deps)?;
            }
            "case" => {
                let child = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                self.collect_expr_deps(root, child, deps)?;
                for arm in payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?
                {
                    // A guard (R14) is evaluated at runtime, so a call inside it is a
                    // real build/link dependency — collect it alongside the body.
                    if let Some(guard) = arm.get("guard").and_then(JsonValue::as_str) {
                        self.collect_expr_deps(root, guard, deps)?;
                    }
                    let child = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            other => bail!("unknown expression kind {other}"),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Number(String),
    String(String),
    ByteString(Vec<u8>),
    Comment(String),
    Symbol(String),
    Eof,
}

#[derive(Default)]
struct ProjectionMetadata {
    type_identity: Option<TypeDefinitionIdentity>,
    member_identity: Option<SymbolBirthSpec>,
}

impl ProjectionMetadata {
    fn is_empty(&self) -> bool {
        self.type_identity.is_none() && self.member_identity.is_none()
    }
}

pub(crate) fn parse_program(source: &str) -> Result<Vec<ProgramItem>> {
    let mut parser = Parser::new(source)?;
    let mut items = Vec::new();
    loop {
        let metadata = parser.take_projection_metadata()?;
        if parser.at_eof_raw() {
            if !metadata.is_empty() {
                bail!("projection identity comment is not attached to a program item");
            }
            break;
        }
        if parser.consume_ident_value("module") {
            if !metadata.is_empty() {
                bail!("projection identity comment cannot attach to module");
            }
            let module = parser.expect_name_path()?;
            parser.expect_symbol("{")?;
            loop {
                let metadata = parser.take_projection_metadata()?;
                if parser.consume_symbol("}") {
                    if !metadata.is_empty() {
                        bail!("projection identity comment cannot attach to module end");
                    }
                    break;
                }
                if parser.at_eof_raw() {
                    bail!("unterminated module {module}");
                }
                items.push(parser.parse_program_item_in_module(module.clone(), metadata)?);
            }
        } else {
            items.push(parser.parse_program_item_in_module(MAIN_BRANCH.to_string(), metadata)?);
        }
    }
    Ok(items)
}

pub(crate) fn parse_expr_source(source: &str) -> Result<RawExpr> {
    let mut parser = Parser::new(source)?;
    let expr = parser.parse_expr()?;
    parser.expect_eof()?;
    Ok(expr)
}

pub(crate) fn parse_signature_source_with_effects(
    source: &str,
) -> Result<(Vec<ParamSpec>, String, Vec<Effect>)> {
    let wrapped = format!("fn __sig__{source} = 0");
    let mut parser = Parser::new(&wrapped)?;
    let function = parser.parse_function()?;
    Ok((function.params, function.return_type, function.effects))
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// When set, a top-level `|` ends the current expression instead of parsing
    /// as bitwise-OR. Set only while parsing a `case`-arm body, so the arm
    /// separator `|` is not swallowed by the (lowest-precedence) bitwise-OR
    /// operator; cleared inside `parse_primary`, so a parenthesized `(a | b)` is
    /// the escape hatch for a bitwise-OR within an arm body.
    bitor_terminates: bool,
}

impl Parser {
    fn new(source: &str) -> Result<Self> {
        Ok(Self {
            tokens: lex(source)?,
            pos: 0,
            bitor_terminates: false,
        })
    }

    fn at_eof(&mut self) -> bool {
        self.skip_comments();
        matches!(self.peek(), Token::Eof)
    }

    fn at_eof_raw(&self) -> bool {
        matches!(self.tokens.get(self.pos).unwrap_or(&Token::Eof), Token::Eof)
    }

    fn expect_eof(&mut self) -> Result<()> {
        if self.at_eof() {
            Ok(())
        } else {
            bail!("unexpected token at end: {:?}", self.peek())
        }
    }

    fn parse_function(&mut self) -> Result<FunctionSource> {
        self.parse_function_in_module(MAIN_BRANCH.to_string())
    }

    fn parse_program_item_in_module(
        &mut self,
        module: String,
        metadata: ProjectionMetadata,
    ) -> Result<ProgramItem> {
        if self.consume_ident_value("extern") {
            if !metadata.is_empty() {
                bail!("projection identity comment cannot attach to extern function");
            }
            Ok(ProgramItem::ExternalFunction(
                self.parse_external_function_in_module(module)?,
            ))
        } else if self.consume_ident_value("record") {
            Ok(ProgramItem::TypeDefinition(
                self.parse_type_definition_in_module(module, "record", metadata)?,
            ))
        } else if self.consume_ident_value("enum") {
            Ok(ProgramItem::TypeDefinition(
                self.parse_type_definition_in_module(module, "enum", metadata)?,
            ))
        } else {
            if !metadata.is_empty() {
                bail!("projection identity comment cannot attach to function");
            }
            Ok(ProgramItem::Function(
                self.parse_function_in_module(module)?,
            ))
        }
    }

    fn parse_type_definition_in_module(
        &mut self,
        module: String,
        kind: &str,
        metadata: ProjectionMetadata,
    ) -> Result<TypeDefinitionSource> {
        if metadata.member_identity.is_some() {
            bail!("member identity comment cannot attach to type definition");
        }
        let name = self.expect_ident()?;
        let (region_params, type_params) = self.parse_optional_region_and_type_params()?;
        self.expect_symbol("{")?;
        let mut members = Vec::new();
        let mut member_births = Vec::new();
        loop {
            let member_metadata = self.take_projection_metadata()?;
            if member_metadata.type_identity.is_some() {
                bail!("type identity comment cannot attach to type member");
            }
            if self.consume_symbol_raw("}") {
                if member_metadata.member_identity.is_some() {
                    bail!("member identity comment cannot attach to type definition end");
                }
                if members.is_empty() {
                    bail!("{kind} definition must have at least one member");
                }
                break;
            }
            let member_name = self.expect_ident()?;
            self.expect_symbol(":")?;
            let ty = self.parse_type_source()?;
            member_births.extend(member_metadata.member_identity);
            members.push(TypeMemberSpec {
                name: member_name,
                ty,
            });
            if self.consume_symbol_raw("}") {
                break;
            }
            let _ = self.consume_symbol_raw(",");
        }
        let definition = match kind {
            "record" => TypeDefinitionKind::Record { fields: members },
            "enum" => TypeDefinitionKind::Enum { variants: members },
            other => bail!("unknown type definition kind {other}"),
        };
        let identity = match metadata.type_identity {
            Some(mut identity) => {
                identity.member_births = member_births;
                Some(identity)
            }
            None if member_births.is_empty() => None,
            None => bail!("member identity comments require a type identity comment"),
        };
        Ok(TypeDefinitionSource {
            module,
            name,
            region_params,
            type_params,
            definition,
            identity,
        })
    }

    fn parse_function_in_module(&mut self, module: String) -> Result<FunctionSource> {
        self.expect_ident_value("fn")?;
        let name = self.expect_ident()?;
        // Generic functions (R11): `<'r, T>` parses into region and type
        // parameters. The type parameters scope the `T` references in the
        // signature/body and drive per-instantiation monomorphization at
        // lowering.
        let (region_params, type_params) = self.parse_optional_region_and_type_params()?;
        let (params, return_type) = self.parse_function_signature_tail()?;
        let effects = if self.consume_ident_value("effects") {
            self.parse_effect_list()?
        } else {
            Vec::new()
        };
        self.expect_symbol("=")?;
        let body = self.parse_expr()?;
        Ok(FunctionSource {
            module,
            name,
            region_params,
            type_params,
            params,
            return_type,
            effects,
            body,
        })
    }

    fn parse_external_function_in_module(
        &mut self,
        module: String,
    ) -> Result<ExternalFunctionSource> {
        self.expect_ident_value("fn")?;
        let name = self.expect_ident()?;
        let region_params = self.parse_optional_region_params()?;
        let (params, return_type) = self.parse_function_signature_tail()?;
        self.expect_ident_value("abi")?;
        let abi = self.parse_bracketed_ident("abi")?;
        let effects = if self.consume_ident_value("effects") {
            self.parse_effect_list()?
        } else {
            Vec::new()
        };
        self.expect_ident_value("link_name")?;
        let link_name = self.expect_string()?;
        let library = if self.consume_ident_value("library") {
            Some(self.expect_string()?)
        } else {
            None
        };
        Ok(ExternalFunctionSource {
            module,
            name,
            region_params,
            params,
            return_type,
            effects,
            abi,
            link_name,
            library,
        })
    }

    fn parse_function_signature_tail(&mut self) -> Result<(Vec<ParamSpec>, String)> {
        self.expect_symbol("(")?;
        let mut params = Vec::new();
        if !self.consume_symbol(")") {
            loop {
                let param_name = self.expect_ident()?;
                self.expect_symbol(":")?;
                let ty = self.parse_type_source()?;
                params.push(ParamSpec {
                    name: param_name,
                    ty,
                });
                if self.consume_symbol(")") {
                    break;
                }
                self.expect_symbol(",")?;
            }
        }
        self.expect_symbol("->")?;
        let return_type = self.parse_type_source()?;
        Ok((params, return_type))
    }

    fn parse_optional_region_params(&mut self) -> Result<Vec<String>> {
        if !self.consume_symbol("<") {
            return Ok(Vec::new());
        }
        let mut params = Vec::new();
        if self.consume_symbol(">") {
            bail!("region parameter list must not be empty");
        }
        loop {
            self.expect_symbol("'")?;
            let name = self.expect_ident()?;
            params.push(name);
            if self.consume_symbol(">") {
                break;
            }
            self.expect_symbol(",")?;
        }
        Ok(params)
    }

    /// Parse an optional definition parameter list `<...>` carrying region
    /// parameters (`'r`) and/or type parameters (bare identifiers), in that order
    /// (R11) — e.g. `<'r, T>`, `<T>`, `<'r>`. Returns `(region_params,
    /// type_params)`. Mirrors `parse_optional_type_args` on the use side so a
    /// definition and its uses share one argument-list grammar.
    fn parse_optional_region_and_type_params(&mut self) -> Result<(Vec<String>, Vec<String>)> {
        if !self.consume_symbol("<") {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut region_params = Vec::new();
        let mut type_params = Vec::new();
        if self.consume_symbol(">") {
            bail!("parameter list must not be empty");
        }
        loop {
            if self.consume_symbol("'") {
                if !type_params.is_empty() {
                    bail!("region parameters must come before type parameters");
                }
                region_params.push(self.expect_ident()?);
            } else {
                type_params.push(self.expect_ident()?);
            }
            if self.consume_symbol(">") {
                break;
            }
            self.expect_symbol(",")?;
        }
        Ok((region_params, type_params))
    }

    fn parse_effect_list(&mut self) -> Result<Vec<Effect>> {
        self.expect_symbol("[")?;
        let mut effects = Vec::new();
        if self.consume_symbol("]") {
            bail!("effect list must not be empty");
        }
        loop {
            let effect = Effect::from_str(&self.expect_ident()?)?;
            effects.push(effect);
            if self.consume_symbol("]") {
                break;
            }
            self.expect_symbol(",")?;
        }
        normalize_effects(&effects)
    }

    fn parse_bracketed_ident(&mut self, label: &str) -> Result<String> {
        self.expect_symbol("[")?;
        let value = self.expect_ident()?;
        self.expect_symbol("]")?;
        if value.is_empty() {
            bail!("{label} must not be empty");
        }
        Ok(value)
    }

    fn parse_expr(&mut self) -> Result<RawExpr> {
        self.parse_let()
    }

    fn parse_let(&mut self) -> Result<RawExpr> {
        if self.consume_ident_value("let") {
            let name = self.expect_ident()?;
            self.expect_symbol(":")?;
            let ty = self.parse_type_source()?;
            self.expect_symbol("=")?;
            let value = self.parse_expr()?;
            self.expect_ident_value("in")?;
            let body = self.parse_expr()?;
            Ok(RawExpr::Let {
                name,
                ty,
                value: Box::new(value),
                body: Box::new(body),
            })
        } else {
            self.parse_return()
        }
    }

    /// `return <expr>` (R7): early exit. A prefix keyword form binding looser than
    /// any operator, so `return` greedily takes the whole following expression
    /// (`return a + b`, `return if c then x else y`). Sits between `let` and `if`
    /// so it is recognized before identifier/operator parsing; in `if c then
    /// return a else b` the `then` branch parses as `return a` and stops at `else`.
    fn parse_return(&mut self) -> Result<RawExpr> {
        if self.consume_ident_value("return") {
            let value = self.parse_expr()?;
            Ok(RawExpr::Return {
                value: Box::new(value),
            })
        } else {
            self.parse_if()
        }
    }

    fn parse_if(&mut self) -> Result<RawExpr> {
        if self.consume_ident_value("if") {
            let cond = self.parse_expr()?;
            self.expect_ident_value("then")?;
            let then_expr = self.parse_expr()?;
            self.expect_ident_value("else")?;
            let else_expr = self.parse_expr()?;
            Ok(RawExpr::If {
                cond: Box::new(cond),
                then_expr: Box::new(then_expr),
                else_expr: Box::new(else_expr),
            })
        } else {
            self.parse_fold()
        }
    }

    fn parse_fold(&mut self) -> Result<RawExpr> {
        if self.consume_ident_value("fold") {
            let item = self.expect_ident()?;
            self.expect_ident_value("in")?;
            let target = self.parse_expr()?;
            self.expect_ident_value("with")?;
            let acc = self.expect_ident()?;
            self.expect_symbol("=")?;
            let init = self.parse_expr()?;
            self.expect_ident_value("do")?;
            let body = self.parse_expr()?;
            Ok(RawExpr::Fold {
                item,
                target: Box::new(target),
                acc,
                init: Box::new(init),
                body: Box::new(body),
            })
        } else {
            self.parse_loop()
        }
    }

    /// `loop <acc> = <init> while <cond> do <body>` (R8): a condition-driven loop
    /// carrying one accumulator. `init`/`cond`/`body` are full expressions; `cond`
    /// stops at `while`/`do` (keywords no operator consumes), so no parens needed.
    fn parse_loop(&mut self) -> Result<RawExpr> {
        if self.consume_ident_value("loop") {
            let acc = self.expect_ident()?;
            self.expect_symbol("=")?;
            let init = self.parse_expr()?;
            self.expect_ident_value("while")?;
            let cond = self.parse_expr()?;
            self.expect_ident_value("do")?;
            let body = self.parse_expr()?;
            Ok(RawExpr::Loop {
                acc,
                init: Box::new(init),
                cond: Box::new(cond),
                body: Box::new(body),
            })
        } else {
            self.parse_case()
        }
    }

    fn parse_case(&mut self) -> Result<RawExpr> {
        if self.consume_ident_value("case") {
            let expr = self.parse_expr()?;
            self.expect_ident_value("of")?;
            let mut arms = Vec::new();
            loop {
                if self.consume_ident_value("else")
                    || self.consume_ident_value("default")
                    || self.consume_ident_value("_")
                {
                    // Wildcard arm (R14 `_`, or the `else`/`default` keyword). With an
                    // `if <guard>` it is a *guarded* wildcard: it may appear before
                    // other arms and never proves exhaustiveness. Without a guard it is
                    // the catch-all default and must be last.
                    let guard = self.parse_optional_case_guard()?;
                    self.expect_symbol("=>")?;
                    let body = self.with_bitor_terminates(true, |p| p.parse_expr())?;
                    let guarded = guard.is_some();
                    arms.push(RawCaseArm {
                        variant: None,
                        literal: None,
                        range: None,
                        default: true,
                        binding: None,
                        guard,
                        payload_pattern: None,
                        body,
                    });
                    if !guarded {
                        if self.consume_symbol("|") {
                            bail!("default case arm must be last");
                        }
                        break;
                    }
                    if !self.consume_symbol("|") {
                        break;
                    }
                } else if let Some(literal) = self.try_parse_scalar_literal_pattern()? {
                    // Scalar literal pattern (R14): `0 => ...`, `true => ...`. An i64
                    // literal may instead open a range pattern `lo..hi` / `lo..=hi`.
                    if self.consume_symbol("..") {
                        let RawExpr::LiteralI64 { .. } = literal else {
                            bail!("range case pattern requires integer bounds");
                        };
                        let inclusive = self.consume_symbol("=");
                        let hi = match self.try_parse_scalar_literal_pattern()? {
                            Some(expr @ RawExpr::LiteralI64 { .. }) => expr,
                            _ => bail!("range case pattern upper bound must be an integer"),
                        };
                        let guard = self.parse_optional_case_guard()?;
                        self.expect_symbol("=>")?;
                        let body = self.with_bitor_terminates(true, |p| p.parse_expr())?;
                        arms.push(RawCaseArm {
                            variant: None,
                            literal: None,
                            range: Some(RawCaseRange {
                                lo: Box::new(literal),
                                hi: Box::new(hi),
                                inclusive,
                            }),
                            default: false,
                            binding: None,
                            guard,
                            payload_pattern: None,
                            body,
                        });
                    } else {
                        let guard = self.parse_optional_case_guard()?;
                        self.expect_symbol("=>")?;
                        let body = self.with_bitor_terminates(true, |p| p.parse_expr())?;
                        arms.push(RawCaseArm {
                            variant: None,
                            literal: Some(Box::new(literal)),
                            range: None,
                            default: false,
                            binding: None,
                            guard,
                            payload_pattern: None,
                            body,
                        });
                    }
                    if !self.consume_symbol("|") {
                        break;
                    }
                } else {
                    let variant = self.expect_ident()?;
                    // Parse the payload pattern (R14): a bare ident `x` binds the
                    // payload; `_` ignores it; a nested `inner(...)` destructures it
                    // recursively. Bare-ident-vs-nullary-variant is disambiguated
                    // syntactically by the parens (`inner(...)` is nested), so a
                    // nullary inner variant is matched with `inner(_)`.
                    let (binding, payload_pattern) = if self.consume_symbol("(") {
                        let pattern = self.parse_case_pattern()?;
                        self.expect_symbol(")")?;
                        match pattern {
                            RawPattern::Binding(name) => (Some(name), None),
                            RawPattern::Wildcard => (None, None),
                            nested @ RawPattern::Variant { .. } => (None, Some(nested)),
                        }
                    } else {
                        (None, None)
                    };
                    let guard = self.parse_optional_case_guard()?;
                    self.expect_symbol("=>")?;
                    let body = self.with_bitor_terminates(true, |p| p.parse_expr())?;
                    arms.push(RawCaseArm {
                        variant: Some(variant),
                        literal: None,
                        range: None,
                        default: false,
                        binding,
                        guard,
                        payload_pattern,
                        body,
                    });
                    if !self.consume_symbol("|") {
                        break;
                    }
                }
            }
            Ok(RawExpr::Case {
                expr: Box::new(expr),
                arms,
            })
        } else {
            self.parse_assignment()
        }
    }

    /// Parse a nested enum-destructuring payload pattern (R14), inside the parens
    /// of a `case` arm (e.g. the `inner(x)` of `some(inner(x))`). A bare ident is a
    /// `Binding`; `_` is a `Wildcard`; an ident immediately followed by `(` is a
    /// nested `Variant` whose sub-pattern is parsed recursively. The presence of
    /// the inner parens is what distinguishes a nested variant from a binding, so a
    /// nullary inner variant is written `inner(_)` (its unit payload ignored).
    fn parse_case_pattern(&mut self) -> Result<RawPattern> {
        if self.consume_ident_value("_") {
            return Ok(RawPattern::Wildcard);
        }
        let ident = self.expect_ident()?;
        if self.consume_symbol("(") {
            let sub = self.parse_case_pattern()?;
            self.expect_symbol(")")?;
            Ok(RawPattern::Variant {
                variant: ident,
                sub: Box::new(sub),
            })
        } else {
            Ok(RawPattern::Binding(ident))
        }
    }

    /// Parse an optional `if <expr>` case-arm guard (R14), consumed after the
    /// pattern and before `=>`. The guard expression stops at `=>` / `|` (neither
    /// is an operator), so a plain `self.parse_expr()` captures exactly the guard.
    fn parse_optional_case_guard(&mut self) -> Result<Option<Box<RawExpr>>> {
        if self.consume_ident_value("if") {
            Ok(Some(Box::new(self.parse_expr()?)))
        } else {
            Ok(None)
        }
    }

    /// Parse a scalar literal case pattern (R14): a decimal integer (optionally
    /// negated) or `true`/`false`. Returns `None` if the next token is not a
    /// literal (so the caller falls back to a variant pattern).
    fn try_parse_scalar_literal_pattern(&mut self) -> Result<Option<RawExpr>> {
        match self.peek() {
            Token::Number(_) => match self.next() {
                Token::Number(value) => Ok(Some(RawExpr::LiteralI64 { value })),
                _ => unreachable!(),
            },
            Token::Symbol(symbol) if symbol == "-" => {
                self.next();
                let value = self.expect_number()?;
                Ok(Some(RawExpr::LiteralI64 {
                    value: format!("-{value}"),
                }))
            }
            Token::Ident(name) if name == "true" || name == "false" => {
                let value = name == "true";
                self.next();
                Ok(Some(RawExpr::LiteralBool { value }))
            }
            _ => Ok(None),
        }
    }

    fn parse_assignment(&mut self) -> Result<RawExpr> {
        let target = self.parse_binary_prec(1)?;
        if self.consume_symbol("=") {
            let value = self.parse_expr()?;
            Ok(RawExpr::Assign {
                target: Box::new(target),
                value: Box::new(value),
            })
        } else {
            Ok(target)
        }
    }

    fn parse_binary_prec(&mut self, min_prec: u8) -> Result<RawExpr> {
        let mut left = self.parse_unary()?;
        loop {
            // The shift operators are two adjacent `<`/`>` tokens (the lexer keeps
            // them separate so nested generic types like `box<Editor<'a>>` still
            // close with single `>`s — type parsing never reaches here). Detect the
            // pair before the single-token comparison operator.
            let first = self.peek().clone();
            let op = match &first {
                Token::Symbol(s) if s == "<" => {
                    if matches!(self.peek_second(), Token::Symbol(t) if t == "<") {
                        "<<".to_string()
                    } else {
                        "<".to_string()
                    }
                }
                Token::Symbol(s) if s == ">" => {
                    if matches!(self.peek_second(), Token::Symbol(t) if t == ">") {
                        ">>".to_string()
                    } else {
                        ">".to_string()
                    }
                }
                // In a `case`-arm body a top-level `|` is the arm separator, not
                // bitwise-OR (which is parenthesized there); end the expression.
                Token::Symbol(s) if s == "|" && self.bitor_terminates => break,
                Token::Symbol(op) if is_binary_op(op) => op.clone(),
                _ => break,
            };
            let prec = op_precedence(&op);
            if prec < min_prec {
                break;
            }
            if op == "<<" || op == ">>" {
                self.next();
                self.next();
            } else {
                self.next();
            }
            let right = self.parse_binary_prec(prec + 1)?;
            left = RawExpr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// The next non-comment token after the current one, for two-token operator
    /// lookahead (`<<` / `>>`). Does not advance the cursor.
    fn peek_second(&mut self) -> Token {
        self.skip_comments();
        let mut j = self.pos + 1;
        while matches!(self.tokens.get(j), Some(Token::Comment(_))) {
            j += 1;
        }
        self.tokens.get(j).cloned().unwrap_or(Token::Eof)
    }

    /// Run `f` with `bitor_terminates` temporarily set to `value`, restoring the
    /// previous value afterward (so nesting composes).
    fn with_bitor_terminates<T>(
        &mut self,
        value: bool,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let saved = self.bitor_terminates;
        self.bitor_terminates = value;
        let result = f(self);
        self.bitor_terminates = saved;
        result
    }

    fn parse_unary(&mut self) -> Result<RawExpr> {
        match self.peek() {
            Token::Symbol(op) if op == "-" || op == "!" || op == "~" => {
                let op = op.clone();
                self.next();
                Ok(RawExpr::Unary {
                    op,
                    expr: Box::new(self.parse_unary()?),
                })
            }
            Token::Symbol(op) if op == "&" => {
                self.next();
                let region = if self.consume_symbol("'") {
                    Some(self.expect_ident()?)
                } else {
                    None
                };
                if self.consume_ident_value("mut") {
                    Ok(RawExpr::BorrowMut {
                        region,
                        target: Box::new(self.parse_unary()?),
                    })
                } else {
                    Ok(RawExpr::BorrowShared {
                        region,
                        target: Box::new(self.parse_unary()?),
                    })
                }
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<RawExpr> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.consume_symbol("[") {
                let index = self.parse_expr()?;
                self.expect_symbol("]")?;
                expr = RawExpr::Index {
                    target: Box::new(expr),
                    index: Box::new(index),
                };
                continue;
            }
            if self.consume_symbol(".") {
                let field = self.expect_ident()?;
                expr = RawExpr::FieldAccess {
                    target: Box::new(expr),
                    field,
                };
                continue;
            }
            break;
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<RawExpr> {
        // A primary is a leaf or a fully-delimited form (parens, call args, array,
        // record, enum payload); none of its sub-expressions is the tail of a
        // `case` arm, so bitwise-OR parses normally inside it. This is what makes
        // `(a | b)` the escape hatch for `|` inside an arm body.
        self.with_bitor_terminates(false, |p| p.parse_primary_inner())
    }

    fn parse_primary_inner(&mut self) -> Result<RawExpr> {
        match self.next() {
            Token::Number(value) => Ok(RawExpr::LiteralI64 { value }),
            Token::String(value) => Ok(RawExpr::LiteralString { value }),
            Token::ByteString(bytes) => Ok(RawExpr::LiteralBytes {
                bytes_hex: bytes_to_hex(&bytes),
            }),
            Token::Ident(name) if name == "true" => Ok(RawExpr::LiteralBool { value: true }),
            Token::Ident(name) if name == "false" => Ok(RawExpr::LiteralBool { value: false }),
            Token::Ident(name) => {
                if name == "enum" && matches!(self.peek(), Token::Symbol(symbol) if symbol == "{") {
                    let enum_type = self.parse_type_source_after_ident(name)?;
                    self.expect_symbol("::")?;
                    let variant = self.expect_ident()?;
                    let value = if self.consume_symbol("(") {
                        let value = self.parse_expr()?;
                        self.expect_symbol(")")?;
                        value
                    } else {
                        RawExpr::Unit
                    };
                    return Ok(RawExpr::EnumConstruct {
                        enum_type,
                        variant,
                        value: Box::new(value),
                    });
                }
                let path = self.finish_name_path(name)?;
                if self.consume_symbol("::") {
                    let variant = self.expect_ident()?;
                    let value = if self.consume_symbol("(") {
                        let value = self.parse_expr()?;
                        self.expect_symbol(")")?;
                        value
                    } else {
                        RawExpr::Unit
                    };
                    Ok(RawExpr::EnumConstruct {
                        enum_type: path,
                        variant,
                        value: Box::new(value),
                    })
                } else if self.consume_symbol("(") {
                    let mut args = Vec::new();
                    if !self.consume_symbol(")") {
                        loop {
                            args.push(self.parse_expr()?);
                            if self.consume_symbol(")") {
                                break;
                            }
                            self.expect_symbol(",")?;
                        }
                    }
                    Ok(RawExpr::Call { name: path, args })
                } else if path.contains('.') {
                    Ok(field_access_from_path(&path))
                } else {
                    Ok(RawExpr::ParamName { name: path })
                }
            }
            Token::Symbol(symbol) if symbol == "(" => {
                if self.consume_symbol(")") {
                    return Ok(RawExpr::Unit);
                }
                let expr = self.parse_expr()?;
                self.expect_symbol(")")?;
                Ok(expr)
            }
            Token::Symbol(symbol) if symbol == "{" => {
                let mut fields = Vec::new();
                if self.consume_symbol("}") {
                    bail!("record literal must have at least one field");
                }
                loop {
                    let name = self.expect_ident()?;
                    self.expect_symbol(":")?;
                    let value = self.parse_expr()?;
                    fields.push(RawRecordField { name, value });
                    if self.consume_symbol("}") {
                        break;
                    }
                    self.expect_symbol(",")?;
                }
                Ok(RawExpr::Record { fields })
            }
            Token::Symbol(symbol) if symbol == "[" => {
                if self.consume_symbol("]") {
                    bail!("array literal must have at least one element");
                }
                let first = self.parse_expr()?;
                // `[value; count]` repeat/fill form (R9): a `;` after the first
                // element switches to the fill grammar; `count` is an integer literal.
                if self.consume_symbol(";") {
                    let RawExpr::LiteralI64 { value: count } = self.parse_expr()? else {
                        bail!("array fill count must be an integer literal");
                    };
                    self.expect_symbol("]")?;
                    return Ok(RawExpr::ArrayFill {
                        value: Box::new(first),
                        count,
                    });
                }
                let mut elements = vec![first];
                if self.consume_symbol("]") {
                    return Ok(RawExpr::Array { elements });
                }
                self.expect_symbol(",")?;
                loop {
                    elements.push(self.parse_expr()?);
                    if self.consume_symbol("]") {
                        break;
                    }
                    self.expect_symbol(",")?;
                }
                Ok(RawExpr::Array { elements })
            }
            other => bail!("unexpected token in expression: {other:?}"),
        }
    }

    fn parse_type_source(&mut self) -> Result<String> {
        match self.next() {
            Token::Symbol(symbol) if symbol == "&" => {
                self.expect_symbol("'")?;
                let region = self.expect_ident()?;
                let mutable = self.consume_ident_value("mut");
                let referent = self.parse_type_source()?;
                if mutable {
                    Ok(format!("&'{region} mut {referent}"))
                } else {
                    Ok(format!("&'{region} {referent}"))
                }
            }
            Token::Ident(name) => self.parse_type_source_after_ident(name),
            Token::Symbol(symbol) if symbol == "(" => {
                self.expect_symbol(")")?;
                Ok("unit".to_string())
            }
            other => bail!("expected type, got {other:?}"),
        }
    }

    fn parse_type_source_after_ident(&mut self, name: String) -> Result<String> {
        if let Some(int) = crate::types::scalar_int_name_for_source(&name) {
            return Ok(int.to_ascii_lowercase());
        }
        match name.as_str() {
            "bool" | "Bool" => Ok("bool".to_string()),
            "unit" | "Unit" => Ok("unit".to_string()),
            "string" | "String" => Ok("string".to_string()),
            "record" => {
                let fields = self.parse_type_fields()?;
                Ok(format!("record {{{}}}", fields.join(", ")))
            }
            "enum" => {
                let variants = self.parse_type_fields()?;
                Ok(format!("enum {{{}}}", variants.join(", ")))
            }
            "raw_ptr" | "raw_mut_ptr" => {
                self.expect_symbol("<")?;
                let pointee = self.parse_type_source()?;
                self.expect_symbol(">")?;
                Ok(format!("{name}<{pointee}>"))
            }
            "box" => {
                self.expect_symbol("<")?;
                let element = self.parse_type_source()?;
                self.expect_symbol(">")?;
                Ok(format!("box<{element}>"))
            }
            "vec" => {
                self.expect_symbol("<")?;
                let element = self.parse_type_source()?;
                self.expect_symbol(">")?;
                Ok(format!("vec<{element}>"))
            }
            "slice" | "mut_slice" => {
                self.expect_symbol("<")?;
                self.expect_symbol("'")?;
                let region = self.expect_ident()?;
                self.expect_symbol(",")?;
                let element = self.parse_type_source()?;
                self.expect_symbol(">")?;
                Ok(format!("{name}<'{region}, {element}>"))
            }
            "array" => {
                self.expect_symbol("<")?;
                let element = self.parse_type_source()?;
                self.expect_symbol(",")?;
                let len = self.expect_number()?;
                self.expect_symbol(">")?;
                Ok(format!("array<{element}, {len}>"))
            }
            _ => {
                let path = self.finish_name_path(name)?;
                let args = self.parse_optional_type_source_args()?;
                if args.is_empty() {
                    Ok(path)
                } else {
                    Ok(format!("{}<{}>", path, args.join(", ")))
                }
            }
        }
    }

    /// Parse a named type's optional argument list `<...>` and re-render each
    /// argument as a source string (R11): a region argument as `'name`, a type
    /// argument by recursively rendering its type. Regions precede types, matching
    /// the type-side grammar, so the rebuilt string re-parses identically.
    fn parse_optional_type_source_args(&mut self) -> Result<Vec<String>> {
        if !self.consume_symbol_raw("<") {
            return Ok(Vec::new());
        }
        let mut args = Vec::new();
        let mut seen_type = false;
        if self.consume_symbol_raw(">") {
            bail!("type/region argument list must not be empty");
        }
        loop {
            if self.consume_symbol_raw("'") {
                if seen_type {
                    bail!("region arguments must come before type arguments");
                }
                args.push(format!("'{}", self.expect_ident()?));
            } else {
                seen_type = true;
                args.push(self.parse_type_source()?);
            }
            if self.consume_symbol_raw(">") {
                break;
            }
            self.expect_symbol(",")?;
        }
        Ok(args)
    }

    fn parse_type_fields(&mut self) -> Result<Vec<String>> {
        self.expect_symbol("{")?;
        let mut fields = Vec::new();
        if self.consume_symbol("}") {
            bail!("type fields must not be empty");
        }
        loop {
            let name = self.expect_ident()?;
            self.expect_symbol(":")?;
            let ty = self.parse_type_source()?;
            fields.push(format!("{name}: {ty}"));
            if self.consume_symbol("}") {
                break;
            }
            self.expect_symbol(",")?;
        }
        fields.sort();
        Ok(fields)
    }

    fn expect_ident(&mut self) -> Result<String> {
        match self.next() {
            Token::Ident(value) => Ok(value),
            other => bail!("expected identifier, got {other:?}"),
        }
    }

    fn expect_number(&mut self) -> Result<String> {
        match self.next() {
            Token::Number(value) => Ok(value),
            other => bail!("expected number, got {other:?}"),
        }
    }

    fn expect_string(&mut self) -> Result<String> {
        match self.next() {
            Token::String(value) => Ok(value),
            other => bail!("expected string literal, got {other:?}"),
        }
    }

    fn expect_name_path(&mut self) -> Result<String> {
        let first = self.expect_ident()?;
        self.finish_name_path(first)
    }

    fn finish_name_path(&mut self, first: String) -> Result<String> {
        let mut parts = vec![first];
        while self.consume_symbol_raw(".") {
            parts.push(self.expect_ident()?);
        }
        Ok(parts.join("."))
    }

    fn expect_ident_value(&mut self, expected: &str) -> Result<()> {
        match self.next() {
            Token::Ident(value) if value == expected => Ok(()),
            other => bail!("expected {expected}, got {other:?}"),
        }
    }

    fn consume_ident_value(&mut self, expected: &str) -> bool {
        match self.peek() {
            Token::Ident(value) if value == expected => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn expect_symbol(&mut self, expected: &str) -> Result<()> {
        match self.next() {
            Token::Symbol(value) if value == expected => Ok(()),
            other => bail!("expected symbol {expected}, got {other:?}"),
        }
    }

    fn consume_symbol(&mut self, expected: &str) -> bool {
        match self.peek() {
            Token::Symbol(value) if value == expected => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn consume_symbol_raw(&mut self, expected: &str) -> bool {
        match self.tokens.get(self.pos).unwrap_or(&Token::Eof) {
            Token::Symbol(value) if value == expected => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn take_projection_metadata(&mut self) -> Result<ProjectionMetadata> {
        let mut metadata = ProjectionMetadata::default();
        while let Some(Token::Comment(text)) = self.tokens.get(self.pos) {
            let text = text.trim().to_string();
            self.pos += 1;
            if let Some(value) = text.strip_prefix("codedb:type_identity ") {
                if metadata.type_identity.is_some() {
                    bail!("duplicate codedb:type_identity comment");
                }
                metadata.type_identity = Some(
                    serde_json::from_str(value)
                        .with_context(|| "invalid codedb:type_identity comment")?,
                );
            } else if let Some(value) = text.strip_prefix("codedb:member_identity ") {
                if metadata.member_identity.is_some() {
                    bail!("duplicate codedb:member_identity comment");
                }
                metadata.member_identity = Some(
                    serde_json::from_str(value)
                        .with_context(|| "invalid codedb:member_identity comment")?,
                );
            }
        }
        Ok(metadata)
    }

    fn skip_comments(&mut self) {
        while matches!(self.tokens.get(self.pos), Some(Token::Comment(_))) {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> &Token {
        self.skip_comments();
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn next(&mut self) -> Token {
        self.skip_comments();
        let token = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        if !matches!(token, Token::Eof) {
            self.pos += 1;
        }
        token
    }
}

fn lex(source: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let chars = source.chars().collect::<Vec<_>>();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == 'b' && i + 1 < chars.len() && chars[i + 1] == '"' {
            let (bytes, next) = lex_byte_string(&chars, i + 1)?;
            tokens.push(Token::ByteString(bytes));
            i = next;
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
        } else if ch.is_ascii_digit() {
            let start = i;
            if ch == '0' && i + 1 < chars.len() && (chars[i + 1] == 'x' || chars[i + 1] == 'X') {
                // Hex literal `0x...` — the number text carries the `0x` prefix and
                // the literal range-check / evaluator parse it (R6, Phase 9).
                i += 2;
                while i < chars.len() && chars[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            tokens.push(Token::Number(chars[start..i].iter().collect()));
        } else if ch == '"' {
            let (value, next) = lex_string(&chars, i)?;
            tokens.push(Token::String(value));
            i = next;
        } else if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            i += 2;
            let start = i;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            tokens.push(Token::Comment(chars[start..i].iter().collect()));
        } else if i + 1 < chars.len() {
            let two = [chars[i], chars[i + 1]].iter().collect::<String>();
            if matches!(
                two.as_str(),
                "->" | "==" | "!=" | "<=" | ">=" | "&&" | "||" | "::" | "=>" | ".."
            ) {
                tokens.push(Token::Symbol(two));
                i += 2;
            } else {
                tokens.push(Token::Symbol(ch.to_string()));
                i += 1;
            }
        } else {
            tokens.push(Token::Symbol(ch.to_string()));
            i += 1;
        }
    }
    tokens.push(Token::Eof);
    Ok(tokens)
}

fn lex_string(chars: &[char], quote: usize) -> Result<(String, usize)> {
    let mut i = quote + 1;
    let mut value = String::new();
    while i < chars.len() {
        match chars[i] {
            '"' => return Ok((value, i + 1)),
            '\\' if i + 1 < chars.len() => {
                let escaped = chars[i + 1];
                match escaped {
                    '"' | '\\' => value.push(escaped),
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    other => bail!("unsupported string escape \\{other}"),
                }
                i += 2;
            }
            ch => {
                value.push(ch);
                i += 1;
            }
        }
    }
    bail!("unterminated string literal")
}

fn lex_byte_string(chars: &[char], quote: usize) -> Result<(Vec<u8>, usize)> {
    let mut i = quote + 1;
    let mut value = Vec::new();
    while i < chars.len() {
        match chars[i] {
            '"' => return Ok((value, i + 1)),
            '\\' if i + 1 < chars.len() => {
                let escaped = chars[i + 1];
                match escaped {
                    '"' => value.push(b'"'),
                    '\\' => value.push(b'\\'),
                    'n' => value.push(b'\n'),
                    't' => value.push(b'\t'),
                    '0' => value.push(0),
                    'x' if i + 3 < chars.len() => {
                        let hi = chars[i + 2];
                        let lo = chars[i + 3];
                        value.push((projection_hex_value(hi)? << 4) | projection_hex_value(lo)?);
                        i += 4;
                        continue;
                    }
                    'x' => bail!("byte escape \\x requires two hex digits"),
                    other => bail!("unsupported byte escape \\{other}"),
                }
                i += 2;
            }
            ch if ch.is_ascii() => {
                value.push(ch as u8);
                i += 1;
            }
            ch => bail!("byte string contains non-ascii character {ch:?}; use \\xNN escapes"),
        }
    }
    bail!("unterminated byte string literal")
}

fn projection_hex_value(ch: char) -> Result<u8> {
    match ch {
        '0'..='9' => Ok((ch as u8) - b'0'),
        'a'..='f' => Ok((ch as u8) - b'a' + 10),
        'A'..='F' => Ok((ch as u8) - b'A' + 10),
        _ => bail!("invalid hex digit {ch:?}"),
    }
}

fn is_binary_op(op: &str) -> bool {
    // The operator registry is the single source of truth for which symbols are
    // binary operators (and their precedence); see `op_registry`.
    crate::op_registry::is_source_binary_op(op)
}

fn local_at_depth<T>(locals: &[T], depth: usize) -> Option<&T> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
}

fn local_at_depth_mut<T>(locals: &mut [T], depth: usize) -> Option<&mut T> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get_mut(idx))
}

pub(crate) fn value_cell(value: Value) -> ValueCell {
    Rc::new(RefCell::new(value))
}

pub(crate) fn semantic_clone_value(value: &Value) -> Value {
    match value {
        Value::I8(value) => Value::I8(*value),
        Value::I16(value) => Value::I16(*value),
        Value::I32(value) => Value::I32(*value),
        Value::I64(value) => Value::I64(*value),
        Value::U8(value) => Value::U8(*value),
        Value::U16(value) => Value::U16(*value),
        Value::U32(value) => Value::U32(*value),
        Value::U64(value) => Value::U64(*value),
        Value::Bool(value) => Value::Bool(*value),
        Value::Unit => Value::Unit,
        Value::SharedRef(value) => Value::SharedRef(value.clone()),
        Value::MutRef(value) => Value::MutRef(value.clone()),
        Value::RawPtr { target, mutable } => Value::RawPtr {
            target: target.clone(),
            mutable: *mutable,
        },
        Value::Boxed(value) => Value::Boxed(value_cell(semantic_clone_value(&value.borrow()))),
        Value::Slice { elements, mutable } => Value::Slice {
            elements: elements.clone(),
            mutable: *mutable,
        },
        Value::Vec { elements, capacity } => Value::Vec {
            elements: elements
                .iter()
                .map(|value| value_cell(semantic_clone_value(&value.borrow())))
                .collect(),
            capacity: *capacity,
        },
        Value::String(bytes) => Value::String(bytes.clone()),
        Value::Array(elements) => Value::Array(
            elements
                .iter()
                .map(|value| value_cell(semantic_clone_value(&value.borrow())))
                .collect(),
        ),
        Value::Record(fields) => Value::Record(
            fields
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        value_cell(semantic_clone_value(&value.borrow())),
                    )
                })
                .collect(),
        ),
        Value::Enum { variant, value } => Value::Enum {
            variant: variant.clone(),
            value: value_cell(semantic_clone_value(&value.borrow())),
        },
    }
}

fn box_payload_cell(value: &ValueCell) -> ValueCell {
    match &*value.borrow() {
        Value::Boxed(payload) => payload.clone(),
        _ => value.clone(),
    }
}

fn eval_index_value(value: Value) -> Result<usize> {
    match value {
        Value::I64(value) if value >= 0 => Ok(value as usize),
        Value::I64(value) => bail!("array index must be non-negative, got {value}"),
        other => bail!("array index evaluated to non-i64 {other}"),
    }
}

pub(crate) fn array_cell(value: &ValueCell, index: usize) -> Result<ValueCell> {
    match &*value.borrow() {
        Value::Slice { elements, .. } => elements
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("slice index {index} out of bounds")),
        Value::Array(elements) => elements
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("array index {index} out of bounds")),
        Value::SharedRef(referent) | Value::MutRef(referent) => array_cell(referent, index),
        other => bail!("array index target evaluated to non-array {other}"),
    }
}

fn array_cell_from_value(value: &Value, index: usize) -> Result<ValueCell> {
    match value {
        Value::Slice { elements, .. } => elements
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("slice index {index} out of bounds")),
        Value::Array(elements) => elements
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("array index {index} out of bounds")),
        Value::SharedRef(referent) | Value::MutRef(referent) => array_cell(referent, index),
        other => bail!("array index target evaluated to non-array {other}"),
    }
}

pub(crate) fn slice_cells_from_array_cell(value: &ValueCell) -> Result<Vec<ValueCell>> {
    match &*value.borrow() {
        Value::Array(elements) => Ok(elements.clone()),
        Value::SharedRef(referent) | Value::MutRef(referent) => {
            slice_cells_from_array_cell(referent)
        }
        other => bail!("slice target evaluated to non-array {other}"),
    }
}

pub(crate) fn slice_len_from_value(value: &Value) -> Result<usize> {
    match value {
        Value::Slice { elements, .. } => Ok(elements.len()),
        other => bail!("len target evaluated to non-slice {other}"),
    }
}

fn bytes_from_slice_value(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::Slice { elements, .. } => elements
            .iter()
            .map(|value| match &*value.borrow() {
                Value::U8(byte) => Ok(*byte),
                other => bail!("string_new source contained non-u8 element {other}"),
            })
            .collect(),
        other => bail!("string_new source evaluated to non-slice {other}"),
    }
}

pub(crate) fn subslice_value(value: &Value, start: usize, len: usize) -> Result<Value> {
    match value {
        Value::Slice { elements, mutable } => {
            let end = start
                .checked_add(len)
                .ok_or_else(|| anyhow!("subslice range overflows"))?;
            if end > elements.len() {
                bail!(
                    "subslice range [{start}, {end}) out of bounds for length {}",
                    elements.len()
                );
            }
            Ok(Value::Slice {
                elements: elements[start..end].to_vec(),
                mutable: *mutable,
            })
        }
        other => bail!("subslice target evaluated to non-slice {other}"),
    }
}

pub(crate) fn field_cell(value: &ValueCell, field: &str) -> Result<ValueCell> {
    match &*value.borrow() {
        Value::Record(fields) => fields
            .get(field)
            .cloned()
            .ok_or_else(|| anyhow!("record value has no field {field}")),
        Value::SharedRef(referent) | Value::MutRef(referent) => field_cell(referent, field),
        other => bail!("field access target evaluated to non-record {other}"),
    }
}
