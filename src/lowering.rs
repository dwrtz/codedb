use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::artifact::CacheKeyInput;
use crate::backend::ArtifactKind;
use crate::model::{ProgramRootPayload, RootSymbolPayload, validate_projection_identifier};
use crate::store::{CodeDb, canonical_json, hash_bytes};
use crate::types::{TypeDefinition, TypeSpec, is_static_region, type_hash_for};
use crate::{BYTES_DOMAIN, DEFAULT_NATIVE_TARGET, MAIN_BRANCH};

pub(crate) const LOWERED_IR_SCHEMA: &str = "codedb/lowered-function-ir/v2";
pub(crate) const LOWERED_DEBUG_MAP_SCHEMA: &str = "codedb/lowered-debug-map/v1";
const LOWERED_IR_INSPECTION_SCHEMA: &str = "codedb/lowered-ir-inspection/v1";
const LOWERING_BACKEND_ID: &str = "lowering-v1";
pub(crate) const LOWERING_TARGET: &str = "target-independent-memory-ir-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredFunctionIr {
    pub(crate) schema: String,
    pub(crate) symbol_hash: String,
    pub(crate) function_def_hash: String,
    pub(crate) function_sig_hash: String,
    pub(crate) typed_body_expr_hash: String,
    pub(crate) params: Vec<LoweredParamSlot>,
    #[serde(default)]
    pub(crate) locals: Vec<LoweredLocalSlot>,
    pub(crate) return_type_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) type_layouts: Vec<LoweredTypeLayout>,
    pub(crate) operations: Vec<LoweredOp>,
    #[serde(default)]
    pub(crate) debug_map: LoweredDebugMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredParamSlot {
    pub(crate) slot: usize,
    pub(crate) type_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredLocalSlot {
    pub(crate) slot: usize,
    pub(crate) type_hash: String,
    #[serde(
        default = "default_slot_size_bytes",
        skip_serializing_if = "is_default_slot_size"
    )]
    pub(crate) size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredTypeLayout {
    pub(crate) type_hash: String,
    pub(crate) kind: String,
    pub(crate) size_bytes: u64,
    pub(crate) align_bytes: u64,
    pub(crate) abi: LoweredTypeAbi,
    #[serde(default)]
    pub(crate) metadata: JsonValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredTypeAbi {
    pub(crate) pass: String,
    #[serde(rename = "return")]
    pub(crate) return_: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredBlock {
    pub(crate) operations: Vec<LoweredOp>,
    pub(crate) result: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredCaseArm {
    pub(crate) variant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) variant_symbol: Option<String>,
    pub(crate) tag_value: u64,
    pub(crate) payload_type_hash: String,
    pub(crate) payload_offset_bytes: u64,
    pub(crate) block: LoweredBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredDebugMap {
    pub(crate) schema: String,
    pub(crate) operations: Vec<LoweredDebugOp>,
    pub(crate) expr_to_ops: Vec<LoweredExprOpMap>,
}

impl Default for LoweredDebugMap {
    fn default() -> Self {
        Self {
            schema: LOWERED_DEBUG_MAP_SCHEMA.to_string(),
            operations: Vec::new(),
            expr_to_ops: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredDebugOp {
    pub(crate) lowered_op_id: String,
    pub(crate) value_id: String,
    pub(crate) lowered_op_kind: String,
    pub(crate) expr_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredExprOpMap {
    pub(crate) expr_hash: String,
    pub(crate) lowered_op_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredTrap {
    pub(crate) condition: String,
    pub(crate) code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LoweredPlace {
    Param {
        slot: usize,
        type_hash: String,
        #[serde(default, skip_serializing_if = "is_false")]
        indirect: bool,
    },
    Local {
        slot: usize,
        type_hash: String,
    },
    Field {
        base: String,
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field_symbol: Option<String>,
        owner_type_hash: String,
        #[serde(default, skip_serializing_if = "is_zero_u64")]
        offset_bytes: u64,
        type_hash: String,
    },
    EnumPayload {
        base: String,
        variant: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variant_symbol: Option<String>,
        owner_type_hash: String,
        tag_value: u64,
        payload_offset_bytes: u64,
        type_hash: String,
    },
    Index {
        base: String,
        index: String,
        element_type_hash: String,
        type_hash: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum LoweredOp {
    /// A direct parameter value. Since the Phase 5 place/address scaffold,
    /// lowering reads parameters through `addr_of_param` + `load` instead, so
    /// this variant is no longer emitted; it is retained (with verifier and
    /// backend support) for reading older lowered-IR artifacts. The
    /// `lowered_memory_ir` regression test asserts current lowering does not
    /// produce it.
    Param {
        id: String,
        slot: usize,
        type_hash: String,
    },
    ConstI64 {
        id: String,
        value: String,
        type_hash: String,
    },
    ConstBool {
        id: String,
        value: bool,
        type_hash: String,
    },
    ConstUnit {
        id: String,
        type_hash: String,
    },
    Unary {
        id: String,
        kind: String,
        value: String,
        type_hash: String,
    },
    /// Sized-integer cast (R6): re-normalize `value` to the canonical slot form of
    /// `type_hash`'s width/signedness (truncate on narrowing, sign-/zero-extend on
    /// widening). The operand is already in its own width's canonical form.
    IntCast {
        id: String,
        value: String,
        type_hash: String,
    },
    Binary {
        id: String,
        kind: String,
        left: String,
        right: String,
        type_hash: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        trap: Option<LoweredTrap>,
    },
    Call {
        id: String,
        target_symbol_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_abi_symbol: Option<String>,
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        return_address: Option<String>,
        type_hash: String,
    },
    If {
        id: String,
        cond: String,
        then_block: LoweredBlock,
        else_block: LoweredBlock,
        type_hash: String,
    },
    Case {
        id: String,
        scrutinee: String,
        enum_type_hash: String,
        arms: Vec<LoweredCaseArm>,
        type_hash: String,
    },
    Fold {
        id: String,
        target_address: String,
        target_type_hash: String,
        len: String,
        init: String,
        index_slot: usize,
        acc_slot: usize,
        item_slot: usize,
        body: LoweredBlock,
        element_type_hash: String,
        acc_type_hash: String,
        type_hash: String,
    },
    /// `loop acc = init while cond do body` (R8): the condition-driven counterpart
    /// of `Fold`. `acc_slot` holds the accumulator (initialized to `init`); each
    /// iteration the `cond` block (reading `acc_slot`) yields a `bool` — if false,
    /// the loop exits with the accumulator as its result (`id`); else the `body`
    /// block (reading `acc_slot`) yields the next accumulator, stored back into
    /// `acc_slot`. Like `Fold`, the accumulator is copyable and the body moves no
    /// owned values (loop-carried drop glue is a follow-on; SPEC_V3 §7).
    Loop {
        id: String,
        acc_slot: usize,
        init: String,
        cond: LoweredBlock,
        body: LoweredBlock,
        acc_type_hash: String,
        type_hash: String,
    },
    BorrowShared {
        id: String,
        address: String,
        region: String,
        referent_type_hash: String,
        type_hash: String,
    },
    BorrowMut {
        id: String,
        address: String,
        region: String,
        referent_type_hash: String,
        type_hash: String,
    },
    DerefShared {
        id: String,
        reference: String,
        referent_type_hash: String,
    },
    DerefMut {
        id: String,
        reference: String,
        referent_type_hash: String,
    },
    DerefBox {
        id: String,
        box_value: String,
        box_type_hash: String,
        element_type_hash: String,
    },
    /// Move the payload out of a `box<T>` and free the box shell (`unbox`, like
    /// `Box::into_inner`). The payload bytes are copied out of the heap into
    /// `dest_slot` (an owned scratch slot) BEFORE `free(box_value)`, so the result
    /// is an independently-owned `T`, not a pointer into freed memory. The box
    /// argument is consumed (moved) by the producing `Move`, so the shell is freed
    /// exactly once and never recursively drops the moved-out payload.
    UnboxMove {
        id: String,
        box_value: String,
        box_type_hash: String,
        element_type_hash: String,
        dest_slot: usize,
    },
    HeapAlloc {
        id: String,
        size_bytes: u64,
        align_bytes: u64,
        element_type_hash: String,
        type_hash: String,
    },
    PtrCast {
        id: String,
        value: String,
        source_type_hash: String,
        type_hash: String,
    },
    DerefRaw {
        id: String,
        pointer: String,
        pointer_type_hash: String,
        pointee_type_hash: String,
        mutable: bool,
    },
    AddrOfParam {
        id: String,
        place: LoweredPlace,
    },
    AddrOfLocal {
        id: String,
        place: LoweredPlace,
    },
    AddrOfField {
        id: String,
        place: LoweredPlace,
    },
    AddrOfEnumPayload {
        id: String,
        place: LoweredPlace,
    },
    AddrOfIndex {
        id: String,
        place: LoweredPlace,
    },
    StaticDataAddress {
        id: String,
        static_data_hash: String,
        bytes_hex: String,
        len: u64,
        element_type_hash: String,
    },
    ConstructSlice {
        id: String,
        address: String,
        data_address: String,
        len: String,
        element_type_hash: String,
        type_hash: String,
    },
    SliceLen {
        id: String,
        slice: String,
        slice_type_hash: String,
        type_hash: String,
    },
    SliceData {
        id: String,
        slice: String,
        slice_type_hash: String,
        element_type_hash: String,
    },
    VecNew {
        id: String,
        address: String,
        capacity: u64,
        element_type_hash: String,
        type_hash: String,
    },
    VecPush {
        id: String,
        vec_address: String,
        value: String,
        vec_type_hash: String,
        element_type_hash: String,
        type_hash: String,
    },
    VecGet {
        id: String,
        vec_address: String,
        index: String,
        vec_type_hash: String,
        element_type_hash: String,
        type_hash: String,
    },
    VecLen {
        id: String,
        vec_address: String,
        vec_type_hash: String,
        type_hash: String,
    },
    StringNew {
        id: String,
        address: String,
        static_data_hash: String,
        bytes_hex: String,
        len: u64,
        type_hash: String,
    },
    StringLen {
        id: String,
        string_address: String,
        string_type_hash: String,
        type_hash: String,
    },
    /// `string_with_capacity(n)` (R15): allocate an empty (len 0) string buffer with
    /// a *runtime* capacity `n` (unlike `VecNew`/`StringNew`, whose capacity is a
    /// compile-time literal). The buffer never reallocs; `StringPush` past `n` traps.
    StringWithCapacity {
        id: String,
        address: String,
        capacity: String,
        type_hash: String,
    },
    /// `string_push(s, b)` (R15): append byte `b` to string place `s` (len += 1),
    /// trapping if it would exceed capacity. The `string` analogue of `VecPush`.
    StringPush {
        id: String,
        string_address: String,
        value: String,
        string_type_hash: String,
        type_hash: String,
    },
    /// `string_get(s, i)` (R15): read byte `i` of string place `s` (bounds-checked
    /// against `len`), yielding a `u8`. The `string` analogue of `VecGet`.
    StringGet {
        id: String,
        string_address: String,
        index: String,
        string_type_hash: String,
        type_hash: String,
    },
    BoundsCheck {
        id: String,
        index: String,
        len: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        len_value: Option<String>,
        type_hash: String,
    },
    SliceRangeCheck {
        id: String,
        start: String,
        len: String,
        source_len: String,
        type_hash: String,
    },
    LoadEnumTag {
        id: String,
        address: String,
        type_hash: String,
    },
    StoreEnumTag {
        address: String,
        type_hash: String,
        variant: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variant_symbol: Option<String>,
        tag_value: u64,
    },
    Load {
        id: String,
        address: String,
        type_hash: String,
    },
    Store {
        address: String,
        value: String,
        type_hash: String,
    },
    Copy {
        id: String,
        value: String,
        type_hash: String,
    },
    Move {
        id: String,
        address: String,
        type_hash: String,
    },
    Drop {
        address: String,
        type_hash: String,
    },
    /// Free a `box`'s heap shell WITHOUT recursively dropping its pointee. Used by
    /// field-granular drop glue when an interior place of the pointee was moved out
    /// (`h.inner` where `h: box<Holder>`): the live siblings are dropped through the
    /// deref by preceding `Drop`s, then this frees the shell. `address` is the
    /// address of the box slot (where the box pointer lives); the backend loads the
    /// pointer, null-checks, and calls `free` — exactly the box drop helper's free
    /// path minus the pointee drop (SPEC_V3 §7). Distinct from `Drop` of a `box`,
    /// which would double-free the moved-out interior.
    FreeBoxShell {
        address: String,
        box_type_hash: String,
    },
    BorrowDebug {
        address: String,
        mutable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        type_hash: String,
    },
    Return {
        value: String,
        type_hash: String,
    },
    /// Early exit (R7): place `value` (of the function's return type) in the
    /// return position and leave the function from here, skipping the rest of the
    /// body. Unlike `Return` (the single, terminal fall-through return) this is a
    /// mid-stream control-flow terminator: it is the last op of a *divergent*
    /// block (an `if`/`case` branch that always `return`s), and the drops for every
    /// owned value live at the exit point are emitted by lowering immediately
    /// before it (SPEC_V3 §7), so each owned value is dropped exactly once on the
    /// early-exit path. The backend reuses the terminal-return value placement and
    /// epilogue, emitted inline (no jump — the epilogue is self-contained).
    EarlyReturn {
        value: String,
        type_hash: String,
    },
}

/// Whether `ops` (a block body) ends by exiting the function early (R7) — its last
/// op is an `EarlyReturn`, or a control-flow op (`If`/`Case`) whose every sub-block
/// diverges. A divergent block never falls through to its `result`/merge, so the
/// `if`/`case` merge (lowering, verify) and the backend skip its result handling,
/// and the function's terminal return/param-drop scaffolds are unreachable after a
/// divergent body. Structural — no IR field — so existing IR hashes are unchanged.
pub(crate) fn lowered_ops_diverge(ops: &[LoweredOp]) -> bool {
    match ops.last() {
        Some(LoweredOp::EarlyReturn { .. }) => true,
        Some(LoweredOp::If {
            then_block,
            else_block,
            ..
        }) => lowered_block_diverges(then_block) && lowered_block_diverges(else_block),
        Some(LoweredOp::Case { arms, .. }) => {
            !arms.is_empty() && arms.iter().all(|arm| lowered_block_diverges(&arm.block))
        }
        _ => false,
    }
}

/// Whether a lowered block exits the function early on every path (R7).
pub(crate) fn lowered_block_diverges(block: &LoweredBlock) -> bool {
    lowered_ops_diverge(&block.operations)
}

pub(crate) struct LoweredFunctionArtifact {
    pub(crate) ir: LoweredFunctionIr,
    pub(crate) lowered_ir_hash: String,
}

struct LoweredExpr {
    operations: Vec<LoweredOp>,
    value: String,
    type_hash: String,
}

struct LoweredAddress {
    operations: Vec<LoweredOp>,
    address: String,
    type_hash: String,
}

struct LoweredFieldInfo {
    type_hash: String,
    field_symbol: Option<String>,
    offset_bytes: u64,
}

struct LoweredVariantInfo {
    type_hash: String,
    variant_symbol: Option<String>,
    tag_value: u64,
    payload_offset_bytes: u64,
}

struct LoweredArrayInfo {
    element_type_hash: String,
    len: u64,
}

#[derive(Debug, Clone)]
struct LocalLoweredBinding {
    slot: usize,
    type_hash: String,
}

/// A scalar `i64` `case` pattern as seen by the desugaring chain (R14): a single
/// literal, a `lo..hi` / `lo..=hi` range, or a guarded wildcard. Each becomes one
/// `if` level whose condition is `scrutinee == lit` / `scrutinee >= lo && scrutinee
/// {<,<=} hi` / (for a wildcard) the guard alone.
enum ScalarArmPattern {
    Literal(String),
    Range {
        lo: String,
        hi: String,
        inclusive: bool,
    },
    /// A *guarded* wildcard (`_ if g`): matches any value, so the arm's condition is
    /// its guard alone. An unguarded wildcard is the chain's terminal `else` and is
    /// never represented as a `Wildcard` entry.
    Wildcard,
}

/// Identifies an addressable owned storage slot (a parameter or a local). Used
/// to track which slots have had their whole value moved out, so the drop
/// scaffold does not drop moved-out (or already-dropped) storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RootSlot {
    Param(usize),
    Local(usize),
}

/// One step of a move-tracked place's path: a record field by name, a fixed
/// array element at a CONSTANT index, or a `box` auto-deref into the pointee.
/// (A dynamic array index is not statically trackable — the drop scaffold could
/// not tell which element survived — so it stays fail-closed and never appears
/// here; see `place_moved_path`.) A `Deref` step reaches a field/element living
/// in a `box`'s heap pointee: the granular drop scaffold reaches the live
/// remainder THROUGH the deref and frees the box shell separately (it cannot use
/// the box's whole-slot drop helper, which would recursively drop the moved-out
/// interior place — SPEC_V3 §7).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PlaceStep {
    Field(String),
    Index(u64),
    Deref,
}

/// A move-tracked place: an owned storage slot plus a projection path into it. An
/// empty path denotes the whole slot. Granular drop glue (Phase 4 + R14) tracks
/// partial moves out of record fields, constant-index array elements, and fields
/// reached through a `box` deref, so the drop scaffold can drop the live remainder
/// of an aggregate while skipping moved-out sub-places. Record-field,
/// constant-array-index, and box-`Deref` steps appear here — dynamic-index and
/// raw-pointer projections are not granular-tracked (their partial move stays
/// fail-closed), so a place rooted in one is never produced.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MovedPlace {
    root: RootSlot,
    path: Vec<PlaceStep>,
}

impl MovedPlace {
    fn whole(root: RootSlot) -> Self {
        Self {
            root,
            path: Vec::new(),
        }
    }
}

/// True when `prefix` denotes `place` or an ancestor of it: same root and a
/// leading field-path prefix (exact field names — no wildcards, records only).
fn place_is_ancestor_or_equal(prefix: &MovedPlace, place: &MovedPlace) -> bool {
    prefix.root == place.root
        && prefix.path.len() <= place.path.len()
        && prefix
            .path
            .iter()
            .zip(place.path.iter())
            .all(|(a, b)| a == b)
}

/// True when the two places overlap — one denotes the other or an ancestor of
/// it (e.g. `x` overlaps `x.a`; `x.a` and `x.b` do not).
fn places_overlap_lowered(left: &MovedPlace, right: &MovedPlace) -> bool {
    place_is_ancestor_or_equal(left, right) || place_is_ancestor_or_equal(right, left)
}

/// Whether `place` (or an ancestor of it) is in `set` — i.e. the move/drop set
/// already covers this place wholesale.
fn place_covered_by(set: &BTreeSet<MovedPlace>, place: &MovedPlace) -> bool {
    set.iter().any(|m| place_is_ancestor_or_equal(m, place))
}

/// Whether any element of `set` overlaps `place` (ancestor, descendant, or
/// equal). Used to reject a move/drop that would conflict with an existing one.
fn place_conflicts(set: &BTreeSet<MovedPlace>, place: &MovedPlace) -> bool {
    set.iter().any(|m| places_overlap_lowered(m, place))
}

/// Insert `place` into a normalized move/drop set: drop any descendants it now
/// subsumes, and skip the insert if an ancestor is already present.
fn insert_moved_place(set: &mut BTreeSet<MovedPlace>, place: MovedPlace) {
    if place_covered_by(set, &place) {
        return;
    }
    set.retain(|m| !place_is_ancestor_or_equal(&place, m));
    set.insert(place);
}

/// Re-normalize a set of places so no element is an ancestor of another (a
/// whole-slot move in one branch subsumes a field move of the same slot from
/// another branch). Order-independent: the broadest place always wins.
fn normalize_moved_set(set: BTreeSet<MovedPlace>) -> BTreeSet<MovedPlace> {
    let mut out = BTreeSet::new();
    for place in set {
        insert_moved_place(&mut out, place);
    }
    out
}

/// The places newly consumed (moved or dropped) by a branch, relative to the
/// state before it. Used by the verifier to merge per-branch move/drop state at
/// an `if`/`case` join (SPEC_V3 §7).
fn newly_consumed_places(
    drop_state: &DropTracker,
    moved_before: &BTreeSet<MovedPlace>,
    dropped_before: &BTreeSet<MovedPlace>,
) -> BTreeSet<MovedPlace> {
    let mut before = moved_before.clone();
    before.extend(dropped_before.iter().cloned());
    let before = normalize_moved_set(before);
    let mut after = drop_state.moved.clone();
    after.extend(drop_state.dropped.iter().cloned());
    normalize_moved_set(after)
        .into_iter()
        .filter(|place| !place_covered_by(&before, place))
        .collect()
}

/// Record as moved every place consumed (moved or dropped) on *every* branch —
/// it is dead after the join. A place consumed on only some branches is a
/// branch-local temporary (slots are unique per function) or is normalized away
/// by the compensating drops the lowering emits, so it is not propagated.
fn merge_consumed_into_moved(drop_state: &mut DropTracker, branch_consumed: &[BTreeSet<MovedPlace>]) {
    let Some((first, rest)) = branch_consumed.split_first() else {
        return;
    };
    for place in first {
        if rest.iter().all(|consumed| place_covered_by(consumed, place)) {
            insert_moved_place(&mut drop_state.moved, place.clone());
        }
    }
}

/// Verifier-side tracking of whole-slot moves and drops, enforcing that an
/// owned slot is never dropped after it was moved out and never dropped twice
/// (SPEC_V2 §20, "drops occur exactly once for owned values"). Address ids and
/// local slots are globally unique within a function, and parameters are only
/// dropped at function end, so a single shared tracker is sound across the
/// `if`/else blocks the verifier recurses into.
#[derive(Debug, Default)]
struct DropTracker {
    /// Address ids that name a tracked owned place (a param/local slot, or a
    /// projection chain of record fields / constant array indices / `box` derefs
    /// rooted in one). Raw-deref and dynamic-index addresses are intentionally
    /// absent — partial moves through them stay fail-closed, so they are never
    /// move/drop-tracked here.
    addr_places: BTreeMap<String, MovedPlace>,
    /// `ConstI64` result id → its constant value, so an `AddrOfIndex` with a constant
    /// index can be resolved to an `Index` step and tracked (a dynamic index is not).
    const_i64: BTreeMap<String, i64>,
    moved: BTreeSet<MovedPlace>,
    dropped: BTreeSet<MovedPlace>,
    /// `AddrOfEnumPayload` result id → (base value, variant). The payload of a
    /// consumed move-only enum is dropped through such an address (e.g. a
    /// `_`/default arm over a `box`-carrying variant frees its payload) — not a
    /// storage-slot place, so it is absent from `addr_places` and would otherwise
    /// escape the at-most-once (double-free) check (SPEC_V3 §7).
    enum_payload_addr: BTreeMap<String, (String, String)>,
    /// (base value, variant) pairs already dropped. Globally unique per function in
    /// well-formed IR — a consumed move-only enum cannot be reused and each variant
    /// is matched in exactly one arm — so a repeat is a double free. No branch
    /// isolation is needed: distinct arms drop distinct variants, and a consumed
    /// scrutinee is never re-`case`d, so a collision can only be a real double drop.
    dropped_enum_payloads: BTreeSet<(String, String)>,
    /// `Load` result id → the tracked place the loaded `box<T>` value came from
    /// (e.g. `h` ⇒ whole slot; `h.b` ⇒ a field chain). Lets a following `DerefBox`
    /// extend the place with a `Deref` step, so a field-granular move/drop reached
    /// through a box deref (`h.inner`) resolves to a tracked place rather than
    /// failing closed at the `Move` check (SPEC_V3 §7).
    loaded_box_place: BTreeMap<String, MovedPlace>,
    /// Box places whose heap shell has been freed (`FreeBoxShell`). Tracked for the
    /// at-most-once (no double-free of the allocation) check; disjoint from the
    /// pointee sub-place drops, which are tracked in `dropped` — freeing the shell
    /// does not conflict with the (legitimate) interior moves/drops that motivated
    /// the granular drop.
    freed_shells: BTreeSet<MovedPlace>,
}

struct LowerCtx {
    target_triple: String,
    /// The enclosing function's declared return type — the type an early `return`
    /// (R7) lowers its operand to, and the `EarlyReturn` op's type. Constant for
    /// the whole body.
    return_type: String,
    next_value: usize,
    next_local: usize,
    local_slots: Vec<LoweredLocalSlot>,
    debug_operations: Vec<LoweredDebugOp>,
    /// Places (whole slots and partial record-field projections) moved out so
    /// far on the current path. Normalized: no element is an ancestor of another.
    moved: BTreeSet<MovedPlace>,
}

impl LowerCtx {
    fn new(target_triple: &str, return_type: &str) -> Self {
        Self {
            target_triple: target_triple.to_string(),
            return_type: return_type.to_string(),
            next_value: 0,
            next_local: 0,
            local_slots: Vec::new(),
            debug_operations: Vec::new(),
            moved: BTreeSet::new(),
        }
    }

    fn target_triple(&self) -> &str {
        &self.target_triple
    }

    /// Record that `place` (a whole slot or a record-field projection) was moved.
    fn mark_moved_place(&mut self, place: MovedPlace) {
        insert_moved_place(&mut self.moved, place);
    }

    /// Whether the whole value of `root` was moved out somewhere in the body.
    fn is_moved(&self, root: RootSlot) -> bool {
        self.moved.contains(&MovedPlace::whole(root))
    }

    fn value(&mut self) -> String {
        let value = format!("v{}", self.next_value);
        self.next_value += 1;
        value
    }

    fn local_slot(&mut self, type_hash: String, size_bytes: u64) -> usize {
        let slot = self.next_local;
        self.next_local += 1;
        self.local_slots.push(LoweredLocalSlot {
            slot,
            type_hash,
            size_bytes,
        });
        slot
    }

    fn push_debug_op(&mut self, expr_hash: &str, lowered_op_kind: &str, value_id: &str) {
        self.debug_operations.push(LoweredDebugOp {
            lowered_op_id: lowered_op_id_for_value(value_id),
            value_id: value_id.to_string(),
            lowered_op_kind: lowered_op_kind.to_string(),
            expr_hash: expr_hash.to_string(),
        });
    }

    fn into_debug_map(self) -> LoweredDebugMap {
        let mut by_expr: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for op in &self.debug_operations {
            by_expr
                .entry(op.expr_hash.clone())
                .or_default()
                .push(op.lowered_op_id.clone());
        }
        LoweredDebugMap {
            schema: LOWERED_DEBUG_MAP_SCHEMA.to_string(),
            operations: self.debug_operations,
            expr_to_ops: by_expr
                .into_iter()
                .map(|(expr_hash, lowered_op_ids)| LoweredExprOpMap {
                    expr_hash,
                    lowered_op_ids,
                })
                .collect(),
        }
    }
}

/// Slots newly moved by an `if` branch (relative to `before`) that are visible
/// outside the branch: parameters, and locals that existed before the branch
/// (slot index `< locals_boundary`). Locals created and moved entirely within
/// the branch are self-contained — their drop is handled inside the branch — so
/// they are excluded and do not count toward the symmetric-move requirement.
fn outer_branch_moves(
    after: &BTreeSet<MovedPlace>,
    before: &BTreeSet<MovedPlace>,
    locals_boundary: usize,
) -> BTreeSet<MovedPlace> {
    after
        .difference(before)
        .filter(|place| match place.root {
            RootSlot::Param(_) => true,
            RootSlot::Local(index) => index < locals_boundary,
        })
        .cloned()
        .collect()
}

impl CodeDb {
    pub fn emit_ir_main_branch(&mut self, function_name: &str) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self
            .resolve_symbol_or_name(&branch.root_hash, function_name)
            .map_err(|err| anyhow!("unknown entry function {function_name}: {err}"))?;
        let artifact = self.lower_symbol(&branch.root_hash, &symbol)?;
        let inspection = json!({
            "schema": LOWERED_IR_INSPECTION_SCHEMA,
            "lowered_ir_hash": artifact.lowered_ir_hash,
            "ir": artifact.ir,
        });
        Ok(format!("{}\n", serde_json::to_string_pretty(&inspection)?))
    }

    pub(crate) fn lower_symbol(
        &mut self,
        root_hash: &str,
        symbol: &str,
    ) -> Result<LoweredFunctionArtifact> {
        let root = self.load_root(root_hash)?;
        let entry = self
            .root_symbol(&root, symbol)
            .cloned()
            .ok_or_else(|| anyhow!("missing symbol {symbol}"))?;
        self.lower_function_definition(&root, &entry)
    }

    pub(crate) fn lower_symbol_for_target(
        &self,
        root_hash: &str,
        symbol: &str,
        target_triple: &str,
    ) -> Result<LoweredFunctionArtifact> {
        let root = self.load_root(root_hash)?;
        let entry = self
            .root_symbol(&root, symbol)
            .cloned()
            .ok_or_else(|| anyhow!("missing symbol {symbol}"))?;
        let ir = self.build_lowered_function_ir(&root, &entry, target_triple)?;
        self.verify_lowered_ir(&root, &ir, target_triple)?;
        let ir_json = serde_json::to_value(&ir)?;
        Ok(LoweredFunctionArtifact {
            ir,
            lowered_ir_hash: hash_lowered_ir_json(&ir_json),
        })
    }

    fn lower_function_definition(
        &mut self,
        root: &ProgramRootPayload,
        entry: &RootSymbolPayload,
    ) -> Result<LoweredFunctionArtifact> {
        let key_input = CacheKeyInput::new(
            ArtifactKind::LoweredIr,
            &entry.definition,
            LOWERING_BACKEND_ID,
            LOWERING_TARGET,
        );
        if let Some(cache_entry) = self.lookup_cache(&key_input)? {
            let artifact_json = cache_entry
                .artifact_json
                .as_ref()
                .ok_or_else(|| anyhow!("lowered IR cache entry missing artifact_json"))?;
            let ir = lowered_ir_from_artifact_metadata(artifact_json)?;
            self.verify_lowered_ir(root, &ir, DEFAULT_NATIVE_TARGET)?;
            let expected = self.build_lowered_function_ir(root, entry, DEFAULT_NATIVE_TARGET)?;
            let ir_json = serde_json::to_value(&ir)?;
            let recomputed_hash = hash_lowered_ir_json(&ir_json);
            if ir != expected || recomputed_hash != cache_entry.artifact_hash {
                return self.write_lowered_ir_artifact(entry, expected);
            }
            return Ok(LoweredFunctionArtifact {
                ir,
                lowered_ir_hash: recomputed_hash,
            });
        }

        let ir = self.build_lowered_function_ir(root, entry, DEFAULT_NATIVE_TARGET)?;
        self.verify_lowered_ir(root, &ir, DEFAULT_NATIVE_TARGET)?;
        self.write_lowered_ir_artifact(entry, ir)
    }

    fn write_lowered_ir_artifact(
        &mut self,
        entry: &RootSymbolPayload,
        ir: LoweredFunctionIr,
    ) -> Result<LoweredFunctionArtifact> {
        let ir_json = serde_json::to_value(&ir)?;
        let lowered_ir_hash = hash_lowered_ir_json(&ir_json);
        self.write_cache_json(
            &entry.definition,
            LOWERING_BACKEND_ID,
            LOWERING_TARGET,
            ArtifactKind::LoweredIr,
            &ir_json,
        )?;
        Ok(LoweredFunctionArtifact {
            ir,
            lowered_ir_hash,
        })
    }

    pub(crate) fn build_lowered_function_ir(
        &self,
        root: &ProgramRootPayload,
        entry: &RootSymbolPayload,
        target_triple: &str,
    ) -> Result<LoweredFunctionIr> {
        if !self.definition_is_internal_function(&entry.definition)? {
            bail!("lowering input is not a FunctionDef {}", entry.definition);
        }
        let definition = self.get_payload(&entry.definition)?;
        let symbol = definition
            .get("symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("function definition missing symbol"))?;
        if symbol != entry.symbol {
            bail!(
                "function definition symbol {} does not match root symbol {}",
                symbol,
                entry.symbol
            );
        }
        let signature = self.function_signature_hash(&entry.definition)?;
        if signature != entry.signature {
            bail!(
                "function definition signature {} does not match root signature {}",
                signature,
                entry.signature
            );
        }
        let (param_types, return_type) = self.signature_parts(&entry.signature)?;
        let allowed_regions = self
            .signature_region_params(&entry.signature)?
            .into_iter()
            .map(|param| param.region)
            .collect::<BTreeSet<_>>();
        for type_hash in &param_types {
            self.ensure_addressable_ir_type(root, type_hash)?;
        }
        self.ensure_lowerable_return_type(root, &return_type)?;
        let body = self.function_body_hash(&entry.definition)?;
        let actual_return = self.verify_expr_type(&body, root, &param_types, &allowed_regions)?;
        if !self.type_assignable_in_root(root, &actual_return, &return_type)? {
            bail!(
                "function body type {} does not match return type {}",
                actual_return,
                return_type
            );
        }

        let mut ctx = LowerCtx::new(target_triple, &return_type);
        let mut lowered = self.lower_expr_as(
            root,
            &body,
            &return_type,
            &param_types,
            &mut ctx,
            &mut Vec::new(),
        )?;
        // Early exit (R7): when the body always `return`s (every path ends in an
        // `EarlyReturn`, which already dropped its live params/locals), the
        // function-end param-drop scaffolds and the terminal `Return` are
        // unreachable. Emitting the param scaffolds would double-drop params on
        // each early-exit path, so skip them; still append a (now unreachable)
        // terminal `Return` to satisfy the "IR ends with a return" invariant.
        let body_diverges = lowered_ops_diverge(&lowered.operations);
        if !body_diverges {
            lowered.operations.extend(self.lower_param_drop_scaffolds(
                root,
                target_triple,
                &param_types,
                &body,
                &mut ctx,
            )?);
        }
        let local_slots = ctx.local_slots.clone();
        let debug_map = ctx.into_debug_map();
        lowered.operations.push(LoweredOp::Return {
            value: lowered.value,
            type_hash: return_type.clone(),
        });
        let type_layouts = self.lowered_type_layouts(
            root,
            target_triple,
            &param_types,
            &return_type,
            &local_slots,
            &lowered.operations,
        )?;

        Ok(LoweredFunctionIr {
            schema: LOWERED_IR_SCHEMA.to_string(),
            symbol_hash: entry.symbol.clone(),
            function_def_hash: entry.definition.clone(),
            function_sig_hash: entry.signature.clone(),
            typed_body_expr_hash: body,
            params: param_types
                .iter()
                .enumerate()
                .map(|(slot, type_hash)| LoweredParamSlot {
                    slot,
                    type_hash: type_hash.clone(),
                })
                .collect(),
            locals: local_slots,
            return_type_hash: return_type,
            type_layouts,
            operations: lowered.operations,
            debug_map,
        })
    }

    fn lower_expr(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let type_hash = expr_type(&payload, expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        match expr_kind {
            "literal_i64" => {
                let value_text = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?;
                // Normalize to the canonical i64 bit pattern (decimal): the literal
                // carries its width in `type_hash` and may be hex, but the backend
                // and constant-index map want one decimal i64 form.
                let int = crate::types::scalar_int_type_by_hash(&type_hash)
                    .ok_or_else(|| anyhow!("integer literal has non-integer type"))?;
                let value = crate::expr::int_literal_const_i64(value_text, int)?.to_string();
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_i64", &id);
                Ok(LoweredExpr {
                    operations: vec![LoweredOp::ConstI64 {
                        id: id.clone(),
                        value,
                        type_hash: type_hash.clone(),
                    }],
                    value: id,
                    type_hash,
                })
            }
            "literal_bool" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?;
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_bool", &id);
                Ok(LoweredExpr {
                    operations: vec![LoweredOp::ConstBool {
                        id: id.clone(),
                        value,
                        type_hash: type_hash.clone(),
                    }],
                    value: id,
                    type_hash,
                })
            }
            "literal_unit" => {
                if type_hash != type_hash_for("Unit") {
                    bail!("literal_unit type mismatch");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_unit", &id);
                Ok(LoweredExpr {
                    operations: vec![LoweredOp::ConstUnit {
                        id: id.clone(),
                        type_hash: type_hash.clone(),
                    }],
                    value: id,
                    type_hash,
                })
            }
            "static_bytes" => self.lower_static_bytes(root, expr_hash, &type_hash, ctx),
            "param_ref" => {
                self.lower_place_value(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "local_ref" => {
                self.lower_place_value(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "call" => {
                let callee_symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                // Generic call (R11): lower it as a call to the monomorphic
                // instance derived from the callee and the concrete type
                // arguments — a distinct native symbol per instantiation. A
                // non-generic call targets its callee directly.
                let type_args = crate::types::call_type_args(&payload)?;
                let target_symbol_hash = if type_args.is_empty() {
                    callee_symbol.to_string()
                } else {
                    crate::types::monomorphic_instance_symbol(callee_symbol, &type_args)
                };
                let target = self
                    .root_symbol(root, &target_symbol_hash)
                    .ok_or_else(|| anyhow!("call target missing from root {target_symbol_hash}"))?;
                let target_abi_symbol = if self.definition_is_external(&target.definition)? {
                    Some(
                        self.external_function_metadata(&target.definition)?
                            .link_name,
                    )
                } else {
                    None
                };
                if self.root_symbol(root, &target_symbol_hash).is_none() {
                    bail!("call target missing from root {target_symbol_hash}");
                }
                let (callee_param_types, _) = self.signature_parts(&target.signature)?;
                let mut operations = Vec::new();
                let mut arg_values = Vec::new();
                for (index, arg) in payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                    .iter()
                    .enumerate()
                {
                    let arg_hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    let lowered = match callee_param_types.get(index) {
                        Some(expected) => {
                            self.lower_expr_as(root, arg_hash, expected, param_types, ctx, locals)?
                        }
                        None => self.lower_expr(root, arg_hash, param_types, ctx, locals)?,
                    };
                    operations.extend(lowered.operations);
                    arg_values.push(lowered.value);
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "call", &id);
                let return_address =
                    if self.type_returns_indirect(root, ctx.target_triple(), &type_hash)? {
                        let slot_size = stack_slot_size_bytes(self.layout_size_bytes(
                            root,
                            ctx.target_triple(),
                            &type_hash,
                        )?);
                        let slot = ctx.local_slot(type_hash.clone(), slot_size);
                        let address = ctx.value();
                        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
                        operations.push(LoweredOp::AddrOfLocal {
                            id: address.clone(),
                            place: LoweredPlace::Local {
                                slot,
                                type_hash: type_hash.clone(),
                            },
                        });
                        Some(address)
                    } else {
                        None
                    };
                operations.push(LoweredOp::Call {
                    id: id.clone(),
                    target_symbol_hash,
                    target_abi_symbol,
                    args: arg_values,
                    return_address,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "binary" => {
                let left_hash = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right_hash = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                let left = self.lower_expr(root, left_hash, param_types, ctx, locals)?;
                let right = self.lower_expr(root, right_hash, param_types, ctx, locals)?;
                let source_op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?;
                let kind =
                    lower_binary_kind(source_op, &left.type_hash, &right.type_hash, &type_hash)?;
                let trap = trap_for_binary(&kind);
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "binary", &id);
                let mut operations = left.operations;
                operations.extend(right.operations);
                operations.push(LoweredOp::Binary {
                    id: id.clone(),
                    kind,
                    left: left.value,
                    right: right.value,
                    type_hash: type_hash.clone(),
                    trap,
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "unary" => {
                let child_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?;
                let child = self.lower_expr(root, child_hash, param_types, ctx, locals)?;
                let source_op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?;
                let kind = lower_unary_kind(source_op, &child.type_hash, &type_hash)?;
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "unary", &id);
                let mut operations = child.operations;
                operations.push(LoweredOp::Unary {
                    id: id.clone(),
                    kind,
                    value: child.value,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "let" => {
                let binding_type = payload
                    .get("binding_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_type"))?
                    .to_string();
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing value"))?;
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing body"))?;
                let slot_size = stack_slot_size_bytes(self.layout_size_bytes(
                    root,
                    ctx.target_triple(),
                    &binding_type,
                )?);
                let slot = ctx.local_slot(binding_type.clone(), slot_size);
                let address = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_local", &address);
                let mut operations = vec![LoweredOp::AddrOfLocal {
                    id: address.clone(),
                    place: LoweredPlace::Local {
                        slot,
                        type_hash: binding_type.clone(),
                    },
                }];
                operations.extend(self.lower_value_into_address(
                    root,
                    value_hash,
                    &binding_type,
                    &address,
                    param_types,
                    ctx,
                    locals,
                )?);
                locals.push(LocalLoweredBinding {
                    slot,
                    type_hash: binding_type.clone(),
                });
                let body = self.lower_expr(root, body_hash, param_types, ctx, locals);
                locals.pop();
                let body = body?;
                operations.extend(body.operations);
                // Drop the binding at scope exit. With field-granular drop glue
                // (SPEC_V3 §7) the binding may be wholly moved (no drop), partly
                // moved (drop the live fields only), or untouched (whole-slot
                // drop). `emit_residual_drops` places exactly the live drops.
                if self.type_requires_drop_scaffold(root, ctx.target_triple(), &binding_type)? {
                    let moved = ctx.moved.clone();
                    let place = MovedPlace::whole(RootSlot::Local(slot));
                    self.emit_residual_drops(
                        root,
                        &place,
                        &binding_type,
                        &binding_type,
                        &moved,
                        body_hash,
                        ctx,
                        &mut operations,
                    )?;
                }
                let _ = address;
                Ok(LoweredExpr {
                    operations,
                    value: body.value,
                    type_hash: body.type_hash,
                })
            }
            "borrow_shared" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing target"))?;
                let region = payload
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing region"))?
                    .to_string();
                let referent_type_hash = payload
                    .get("referent_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing referent_type"))?
                    .to_string();
                let target_payload = self.get_payload(target_hash)?;
                let target_type = expr_type(&target_payload, target_hash)?;
                let target = match self.type_spec(&target_type)? {
                    TypeSpec::Box { element } if element == referent_type_hash => self
                        .lower_box_payload_place(
                            root,
                            target_hash,
                            &target_type,
                            &element,
                            expr_hash,
                            param_types,
                            ctx,
                            locals,
                        )?,
                    _ => self.lower_place(root, target_hash, param_types, ctx, locals)?,
                };
                if target.type_hash != referent_type_hash {
                    bail!("borrow_shared referent type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "borrow_shared", &id);
                let mut operations = target.operations;
                operations.push(LoweredOp::BorrowDebug {
                    address: target.address.clone(),
                    mutable: false,
                    region: Some(region.clone()),
                    type_hash: referent_type_hash.clone(),
                });
                operations.push(LoweredOp::BorrowShared {
                    id: id.clone(),
                    address: target.address,
                    region,
                    referent_type_hash,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "borrow_mut" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing target"))?;
                let region = payload
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing region"))?
                    .to_string();
                let referent_type_hash = payload
                    .get("referent_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing referent_type"))?
                    .to_string();
                let target_payload = self.get_payload(target_hash)?;
                let target_type = expr_type(&target_payload, target_hash)?;
                let target = match self.type_spec(&target_type)? {
                    TypeSpec::Box { element } if element == referent_type_hash => self
                        .lower_box_payload_place(
                            root,
                            target_hash,
                            &target_type,
                            &element,
                            expr_hash,
                            param_types,
                            ctx,
                            locals,
                        )?,
                    _ => self.lower_place(root, target_hash, param_types, ctx, locals)?,
                };
                if target.type_hash != referent_type_hash {
                    bail!("borrow_mut referent type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "borrow_mut", &id);
                let mut operations = target.operations;
                operations.push(LoweredOp::BorrowDebug {
                    address: target.address.clone(),
                    mutable: true,
                    region: Some(region.clone()),
                    type_hash: referent_type_hash.clone(),
                });
                operations.push(LoweredOp::BorrowMut {
                    id: id.clone(),
                    address: target.address,
                    region,
                    referent_type_hash,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "slice_from_array" => {
                self.lower_slice_from_array(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "slice_len" => self.lower_slice_len(root, expr_hash, param_types, ctx, locals),
            "subslice" => {
                self.lower_subslice(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "box_new" => self.lower_box_new(root, expr_hash, &type_hash, param_types, ctx, locals),
            "unbox" => self.lower_unbox(root, expr_hash, &type_hash, param_types, ctx, locals),
            "int_cast" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing value"))?;
                let source = self.lower_expr(root, value_hash, param_types, ctx, locals)?;
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "int_cast", &id);
                let mut operations = source.operations;
                operations.push(LoweredOp::IntCast {
                    id: id.clone(),
                    value: source.value,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "vec_new" => self.lower_vec_new(root, expr_hash, &type_hash, param_types, ctx, locals),
            "vec_push" => self.lower_vec_push(root, expr_hash, param_types, ctx, locals),
            "vec_get" => self.lower_vec_get(root, expr_hash, &type_hash, param_types, ctx, locals),
            "vec_len" => self.lower_vec_len(root, expr_hash, param_types, ctx, locals),
            "string_new" => self.lower_string_new(root, expr_hash, &type_hash, ctx),
            "string_len" => self.lower_string_len(root, expr_hash, param_types, ctx, locals),
            "string_with_capacity" => {
                self.lower_string_with_capacity(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "string_push" => self.lower_string_push(root, expr_hash, param_types, ctx, locals),
            "string_get" => {
                self.lower_string_get(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "raw_ptr_cast" => {
                self.lower_raw_ptr_cast(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "raw_load" => self.lower_raw_load(root, expr_hash, param_types, ctx, locals),
            "raw_store" => self.lower_raw_store(root, expr_hash, param_types, ctx, locals),
            "assign" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("assign missing target"))?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("assign missing value"))?;
                let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
                let target_type = target.type_hash.clone();
                let target_address = target.address.clone();
                let target_root_slot = self.place_whole_root_slot(target_hash, locals)?;
                let target_place = self.place_moved_path(target_hash, locals)?;
                let mut operations = target.operations;
                let value =
                    self.lower_expr_as(root, value_hash, &target_type, param_types, ctx, locals)?;
                if !self.type_assignable_in_root(root, &value.type_hash, &target_type)? {
                    bail!("assignment type mismatch while lowering");
                }
                if let Some(root_slot) = target_root_slot
                    && ctx.is_moved(root_slot)
                {
                    bail!("assignment target was moved while lowering assignment value");
                }
                operations.extend(value.operations);
                // Drop the overwritten value before the store. If the target was
                // partially moved (some fields already gone), drop only the live
                // remainder (field-granular drop glue, SPEC_V3 §7).
                if self.type_requires_drop_scaffold(root, ctx.target_triple(), &target_type)? {
                    if let Some(place) = &target_place {
                        let moved = ctx.moved.clone();
                        let root_type = self.root_slot_type(place.root, param_types, ctx)?;
                        let place = place.clone();
                        self.emit_residual_drops(
                            root,
                            &place,
                            &target_type,
                            &root_type,
                            &moved,
                            target_hash,
                            ctx,
                            &mut operations,
                        )?;
                    } else {
                        operations.push(LoweredOp::Drop {
                            address: target_address.clone(),
                            type_hash: target_type.clone(),
                        });
                    }
                }
                operations.push(LoweredOp::Store {
                    address: target_address,
                    value: value.value,
                    type_hash: target_type.clone(),
                });
                // The store re-initializes the target place: clear any move
                // recorded for it (and overlapping sub/super-places).
                if let Some(place) = &target_place {
                    ctx.moved.retain(|m| !places_overlap_lowered(place, m));
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_unit", &id);
                operations.push(LoweredOp::ConstUnit {
                    id: id.clone(),
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "record_literal" => self.lower_record_literal_into_slot(
                root,
                expr_hash,
                &type_hash,
                param_types,
                ctx,
                locals,
            ),
            "array_literal" | "array_fill" | "array_set" => self.lower_array_literal_into_slot(
                root,
                expr_hash,
                &type_hash,
                param_types,
                ctx,
                locals,
            ),
            "if" => self.lower_if(root, expr_hash, &type_hash, param_types, ctx, locals),
            "return" => self.lower_return(root, expr_hash, param_types, ctx, locals),
            "field_access" => {
                self.lower_place_value(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "array_index" => {
                self.lower_place_value(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "enum_construct" => self.lower_enum_construct_into_slot(
                root,
                expr_hash,
                &type_hash,
                param_types,
                ctx,
                locals,
            ),
            "case" => self.lower_case(root, expr_hash, &type_hash, param_types, ctx, locals),
            "fold" => self.lower_fold(root, expr_hash, &type_hash, param_types, ctx, locals),
            "loop" => self.lower_loop(root, expr_hash, &type_hash, param_types, ctx, locals),
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn lower_param_drop_scaffolds(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        param_types: &[String],
        debug_expr_hash: &str,
        ctx: &mut LowerCtx,
    ) -> Result<Vec<LoweredOp>> {
        let _ = target_triple;
        let mut operations = Vec::new();
        for (slot, type_hash) in param_types.iter().enumerate() {
            if !self.type_requires_drop_scaffold(root, ctx.target_triple(), type_hash)? {
                continue;
            }
            // Drop the live remainder of the parameter at function end: nothing
            // if wholly moved, the live fields if partly moved (SPEC_V3 §7).
            let moved = ctx.moved.clone();
            let type_hash = type_hash.clone();
            let place = MovedPlace::whole(RootSlot::Param(slot));
            self.emit_residual_drops(
                root,
                &place,
                &type_hash,
                &type_hash,
                &moved,
                debug_expr_hash,
                ctx,
                &mut operations,
            )?;
        }
        Ok(operations)
    }

    /// If `expr_hash` is a bare parameter or local reference (a whole owned
    /// slot, not a field/index/deref projection), return the slot it names.
    /// Moving such a place moves the entire slot, so its drop scaffold must be
    /// suppressed. Projections return `None` (partial moves are not tracked at
    /// this scaffold layer).
    fn place_whole_root_slot(
        &self,
        expr_hash: &str,
        locals: &[LocalLoweredBinding],
    ) -> Result<Option<RootSlot>> {
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("param_ref") => {
                let slot = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                Ok(Some(RootSlot::Param(slot)))
            }
            Some("local_ref") => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                let binding = local_lowered_at_depth(locals, depth)
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds {depth}"))?;
                Ok(Some(RootSlot::Local(binding.slot)))
            }
            _ => Ok(None),
        }
    }

    /// Resolve the move-tracked place a value expression denotes: a whole slot
    /// (param/local) or a projection chain (record fields and constant-index array
    /// elements) into one. Returns `None` for a dynamic array index, a box payload,
    /// or a raw deref — partial moves through those are not granular-tracked and stay
    /// fail-closed.
    fn place_moved_path(
        &self,
        expr_hash: &str,
        locals: &[LocalLoweredBinding],
    ) -> Result<Option<MovedPlace>> {
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("param_ref") => {
                let slot = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                Ok(Some(MovedPlace::whole(RootSlot::Param(slot))))
            }
            Some("local_ref") => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                let binding = local_lowered_at_depth(locals, depth)
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds {depth}"))?;
                Ok(Some(MovedPlace::whole(RootSlot::Local(binding.slot))))
            }
            Some("field_access") => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                let field = payload
                    .get("field")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing field"))?;
                match self.place_moved_path(target_hash, locals)? {
                    Some(mut base) => {
                        // A field reached through a `box` auto-deref lives in the
                        // box's heap pointee, not the box slot: record a `Deref`
                        // step so the granular drop reaches the live siblings
                        // through the deref and frees the shell separately
                        // (SPEC_V3 §7).
                        if self.place_target_is_box(target_hash)? {
                            base.path.push(PlaceStep::Deref);
                        }
                        base.path.push(PlaceStep::Field(field.to_string()));
                        Ok(Some(base))
                    }
                    None => Ok(None),
                }
            }
            Some("array_index") => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let index_hash = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing index"))?;
                // Only a CONSTANT non-negative index is element-granular-trackable; a
                // dynamic index stays fail-closed (the static scaffold cannot know which
                // element survived). A constant index is in-bounds (type-check rejects an
                // out-of-bounds literal). A slice index (no fixed length) also bails to
                // `None` — `place_moved_path` only reaches here for a fixed array, whose
                // base resolves to a tracked slot.
                match (
                    self.literal_i64_value(index_hash)?,
                    self.place_moved_path(target_hash, locals)?,
                ) {
                    (Some(index), Some(mut base)) if index >= 0 => {
                        // A `box<[T; N]>` indexed directly auto-derefs into the
                        // pointee, exactly like a box field access (see above).
                        if self.place_target_is_box(target_hash)? {
                            base.path.push(PlaceStep::Deref);
                        }
                        base.path.push(PlaceStep::Index(index as u64));
                        Ok(Some(base))
                    }
                    _ => Ok(None),
                }
            }
            // raw deref, etc.: not granular-tracked.
            _ => Ok(None),
        }
    }

    /// Whether `target_hash`'s declared type is a `box<T>` — i.e. a field/index
    /// access on it auto-derefs into the heap pointee (granular drop reaches it
    /// through a `Deref` step; SPEC_V3 §7).
    fn place_target_is_box(&self, target_hash: &str) -> Result<bool> {
        let payload = self.get_payload(target_hash)?;
        let target_type = expr_type(&payload, target_hash)?;
        Ok(matches!(self.type_spec(&target_type)?, TypeSpec::Box { .. }))
    }

    /// Resolve the place a move-only value is moved out of, recording it in the
    /// move set. A whole slot, a record-field projection, or a constant-index array
    /// element is tracked (the enclosing aggregate's drop is then granular — see
    /// `emit_residual_drops`). A dynamic-index/box/deref projection cannot be dropped
    /// granularly, so a partial move through one stays fail-closed (SPEC_V3 §7).
    /// `verify_lowered_ir`'s `Move` check is the independent backstop.
    fn mark_moved_source(
        &self,
        expr_hash: &str,
        ctx: &mut LowerCtx,
        locals: &[LocalLoweredBinding],
    ) -> Result<()> {
        let place = self.place_moved_path(expr_hash, locals)?.ok_or_else(|| {
            anyhow!(
                "unsupported_move: lowering does not support moving a move-only value out of a dynamic array index or pointer projection (partial move of an owned aggregate); granular drop glue covers record fields and constant-index array elements only (SPEC_V3 §7)"
            )
        })?;
        ctx.mark_moved_place(place);
        Ok(())
    }

    /// Emit the address-of operations for a tracked place (whole slot or
    /// record-field chain), returning the final address id and the place's type.
    #[allow(clippy::too_many_arguments)]
    fn emit_place_address(
        &self,
        root: &ProgramRootPayload,
        place: &MovedPlace,
        root_type: &str,
        debug_expr_hash: &str,
        ctx: &mut LowerCtx,
        operations: &mut Vec<LoweredOp>,
    ) -> Result<(String, String)> {
        let target_triple = ctx.target_triple().to_string();
        let mut address = ctx.value();
        match place.root {
            RootSlot::Param(slot) => {
                ctx.push_debug_op(debug_expr_hash, "addr_of_param", &address);
                operations.push(LoweredOp::AddrOfParam {
                    id: address.clone(),
                    place: LoweredPlace::Param {
                        slot,
                        type_hash: root_type.to_string(),
                        indirect: self.type_passes_indirect(root, &target_triple, root_type)?,
                    },
                });
            }
            RootSlot::Local(slot) => {
                ctx.push_debug_op(debug_expr_hash, "addr_of_local", &address);
                operations.push(LoweredOp::AddrOfLocal {
                    id: address.clone(),
                    place: LoweredPlace::Local {
                        slot,
                        type_hash: root_type.to_string(),
                    },
                });
            }
        }
        let mut current_type = root_type.to_string();
        for step in &place.path {
            match step {
                PlaceStep::Field(field) => {
                    let field_info =
                        self.lowered_record_field(root, &target_triple, &current_type, field)?;
                    let next = ctx.value();
                    ctx.push_debug_op(debug_expr_hash, "addr_of_field", &next);
                    operations.push(LoweredOp::AddrOfField {
                        id: next.clone(),
                        place: LoweredPlace::Field {
                            base: address,
                            field: field.clone(),
                            field_symbol: field_info.field_symbol,
                            owner_type_hash: current_type.clone(),
                            offset_bytes: field_info.offset_bytes,
                            type_hash: field_info.type_hash.clone(),
                        },
                    });
                    address = next;
                    current_type = field_info.type_hash;
                }
                PlaceStep::Deref => {
                    // A `box` auto-deref: load the box pointer out of the slot, then
                    // `DerefBox` it to the pointee address — mirroring `box` field
                    // read lowering (`lower_box_payload_place`). The subsequent
                    // field/index steps address into the heap pointee, and the
                    // box's shell is freed by a sibling `FreeBoxShell`.
                    let element_type = match self.type_spec_in_root(root, &current_type)? {
                        TypeSpec::Box { element } => element,
                        other => bail!(
                            "granular drop deref step requires a box type, got {}",
                            other.to_source(self)?
                        ),
                    };
                    let box_value = ctx.value();
                    ctx.push_debug_op(debug_expr_hash, "load", &box_value);
                    operations.push(LoweredOp::Load {
                        id: box_value.clone(),
                        address,
                        type_hash: current_type.clone(),
                    });
                    let next = ctx.value();
                    ctx.push_debug_op(debug_expr_hash, "deref_box", &next);
                    operations.push(LoweredOp::DerefBox {
                        id: next.clone(),
                        box_value,
                        box_type_hash: current_type.clone(),
                        element_type_hash: element_type.clone(),
                    });
                    address = next;
                    current_type = element_type;
                }
                PlaceStep::Index(index) => {
                    // A constant array index: materialize it and address the element,
                    // mirroring `arr[N]` place lowering (the element drop reaches it).
                    let array_info =
                        self.lowered_array_info(root, &target_triple, &current_type)?;
                    let index_id = ctx.value();
                    ctx.push_debug_op(debug_expr_hash, "const_i64", &index_id);
                    operations.push(LoweredOp::ConstI64 {
                        id: index_id.clone(),
                        value: index.to_string(),
                        type_hash: type_hash_for("I64"),
                    });
                    let next = ctx.value();
                    ctx.push_debug_op(debug_expr_hash, "addr_of_index", &next);
                    operations.push(LoweredOp::AddrOfIndex {
                        id: next.clone(),
                        place: LoweredPlace::Index {
                            base: address,
                            index: index_id,
                            element_type_hash: array_info.element_type_hash.clone(),
                            type_hash: array_info.element_type_hash.clone(),
                        },
                    });
                    address = next;
                    current_type = array_info.element_type_hash;
                }
            }
        }
        Ok((address, current_type))
    }

    /// Drop the live remainder of `place` (type `place_type`): every drop-needing
    /// sub-place not already covered by `moved`. A fully-moved place emits
    /// nothing; a place with some moved fields recurses into the record and drops
    /// only the live fields (field-granular drop glue, SPEC_V3 §7); an
    /// untouched place emits a single whole-slot drop (the backend's per-type
    /// drop helper recurses into its fields).
    #[allow(clippy::too_many_arguments)]
    fn emit_residual_drops(
        &self,
        root: &ProgramRootPayload,
        place: &MovedPlace,
        place_type: &str,
        root_type: &str,
        moved: &BTreeSet<MovedPlace>,
        debug_expr_hash: &str,
        ctx: &mut LowerCtx,
        operations: &mut Vec<LoweredOp>,
    ) -> Result<()> {
        let target_triple = ctx.target_triple().to_string();
        if place_covered_by(moved, place) {
            return Ok(());
        }
        let has_inner_move = moved
            .iter()
            .any(|m| place_is_ancestor_or_equal(place, m) && m != place);
        if !has_inner_move {
            if self.type_requires_drop_scaffold(root, &target_triple, place_type)? {
                let (address, _) =
                    self.emit_place_address(root, place, root_type, debug_expr_hash, ctx, operations)?;
                operations.push(LoweredOp::Drop {
                    address,
                    type_hash: place_type.to_string(),
                });
            }
            return Ok(());
        }
        // Some sub-place of `place` was moved; drop the live sub-places individually.
        // Only records (by field) and fixed arrays (by constant index) are
        // granular-tracked, so `place_type` is one of those.
        match self.type_spec_in_root(root, place_type)? {
            TypeSpec::Record(fields) => {
                for field in fields {
                    let mut child = place.clone();
                    child.path.push(PlaceStep::Field(field.name.clone()));
                    self.emit_residual_drops(
                        root,
                        &child,
                        &field.type_hash,
                        root_type,
                        moved,
                        debug_expr_hash,
                        ctx,
                        operations,
                    )?;
                }
            }
            TypeSpec::FixedArray { element, len } => {
                for index in 0..len {
                    let mut child = place.clone();
                    child.path.push(PlaceStep::Index(index));
                    self.emit_residual_drops(
                        root,
                        &child,
                        &element,
                        root_type,
                        moved,
                        debug_expr_hash,
                        ctx,
                        operations,
                    )?;
                }
            }
            TypeSpec::Box { element } => {
                // The box's pointee was partially moved: drop the live remainder of
                // the pointee THROUGH the deref, then free the box shell. The
                // whole-box drop helper cannot be used — it recurses into the
                // moved-out interior place (double free). Read-before-free order:
                // the pointee field drops precede the shell free (SPEC_V3 §7).
                let mut child = place.clone();
                child.path.push(PlaceStep::Deref);
                self.emit_residual_drops(
                    root,
                    &child,
                    &element,
                    root_type,
                    moved,
                    debug_expr_hash,
                    ctx,
                    operations,
                )?;
                let (address, _) = self.emit_place_address(
                    root,
                    place,
                    root_type,
                    debug_expr_hash,
                    ctx,
                    operations,
                )?;
                operations.push(LoweredOp::FreeBoxShell {
                    address,
                    box_type_hash: place_type.to_string(),
                });
            }
            other => bail!(
                "granular drop requires a record, array, or box type, got {}",
                other.to_source(self)?
            ),
        }
        Ok(())
    }

    /// At a branch merge, drop in this branch the parts of `place` that the
    /// merged post-state (`dead`) treats as moved but this branch left live
    /// (`branch_moved`). This normalizes every branch to the same move set so a
    /// single static drop scaffold stays exactly-once across conditional moves
    /// (SPEC_V3 §7) — no runtime drop flags. Reduces to `emit_residual_drops`
    /// when `dead` covers `place` wholesale.
    #[allow(clippy::too_many_arguments)]
    fn emit_branch_compensation(
        &self,
        root: &ProgramRootPayload,
        place: &MovedPlace,
        place_type: &str,
        root_type: &str,
        dead: &BTreeSet<MovedPlace>,
        branch_moved: &BTreeSet<MovedPlace>,
        debug_expr_hash: &str,
        ctx: &mut LowerCtx,
        operations: &mut Vec<LoweredOp>,
    ) -> Result<()> {
        if place_covered_by(branch_moved, place) {
            return Ok(());
        }
        if place_covered_by(dead, place) {
            // `dead` wants the whole place gone; this branch kept it (modulo its
            // own sub-moves) live — drop the residual.
            return self.emit_residual_drops(
                root,
                place,
                place_type,
                root_type,
                branch_moved,
                debug_expr_hash,
                ctx,
                operations,
            );
        }
        // `dead` neither covers `place` nor any sub-field of it: this branch's
        // view of `place` already matches the merged state (the merge `dead` set
        // is the union of every branch's moves, so `branch_moved ⊆ dead` — an
        // untouched-by-`dead` place is untouched by this branch too). Nothing to
        // compensate. Mirror of `emit_residual_drops`' `has_inner_move` guard;
        // without it an untouched non-record sibling (a `box`/scalar field left
        // live while another field is conditionally moved) is wrongly recursed
        // into as a record and `aggregate_record_fields` bails (SPEC_V3 §7).
        let has_inner_dead = dead
            .iter()
            .any(|m| place_is_ancestor_or_equal(place, m) && m != place);
        if !has_inner_dead {
            return Ok(());
        }
        // `dead` marks sub-places of `place`; recurse. Only records (by field) and
        // fixed arrays (by constant index) are granular-tracked (box/deref and
        // dynamic-index partial moves stay fail-closed), so a place with an
        // inner-dead sub-place is one of those.
        match self.type_spec_in_root(root, place_type)? {
            TypeSpec::Record(fields) => {
                for field in fields {
                    let mut child = place.clone();
                    child.path.push(PlaceStep::Field(field.name.clone()));
                    self.emit_branch_compensation(
                        root,
                        &child,
                        &field.type_hash,
                        root_type,
                        dead,
                        branch_moved,
                        debug_expr_hash,
                        ctx,
                        operations,
                    )?;
                }
            }
            TypeSpec::FixedArray { element, len } => {
                for index in 0..len {
                    let mut child = place.clone();
                    child.path.push(PlaceStep::Index(index));
                    self.emit_branch_compensation(
                        root,
                        &child,
                        &element,
                        root_type,
                        dead,
                        branch_moved,
                        debug_expr_hash,
                        ctx,
                        operations,
                    )?;
                }
            }
            TypeSpec::Box { element } => {
                // Compensate moved pointee fields THROUGH the deref so every branch
                // agrees on the pointee's move state. The shell is NOT freed here —
                // the box is freed exactly once by the scope-exit residual drop
                // (this only normalizes the per-branch move set; SPEC_V3 §7).
                let mut child = place.clone();
                child.path.push(PlaceStep::Deref);
                self.emit_branch_compensation(
                    root,
                    &child,
                    &element,
                    root_type,
                    dead,
                    branch_moved,
                    debug_expr_hash,
                    ctx,
                    operations,
                )?;
            }
            other => bail!(
                "granular compensation requires a record, array, or box type, got {}",
                other.to_source(self)?
            ),
        }
        Ok(())
    }

    /// The type of an outer root slot (param or pre-branch local), used to root
    /// residual/compensation drops at a merge.
    fn root_slot_type(
        &self,
        slot: RootSlot,
        param_types: &[String],
        ctx: &LowerCtx,
    ) -> Result<String> {
        match slot {
            RootSlot::Param(index) => param_types
                .get(index)
                .cloned()
                .ok_or_else(|| anyhow!("compensation drop references unknown param slot {index}")),
            RootSlot::Local(index) => ctx
                .local_slots
                .iter()
                .find(|local| local.slot == index)
                .map(|local| local.type_hash.clone())
                .ok_or_else(|| anyhow!("compensation drop references unknown local slot {index}")),
        }
    }

    /// Emit compensating drops so each branch of an `if`/`case` exits with the
    /// same move set (`union`). For every outer root slot touched by the union
    /// but left (wholly or partly) live by this branch, drop the residual.
    #[allow(clippy::too_many_arguments)]
    fn emit_merge_compensation(
        &self,
        root: &ProgramRootPayload,
        union: &BTreeSet<MovedPlace>,
        branch_moved: &BTreeSet<MovedPlace>,
        param_types: &[String],
        debug_expr_hash: &str,
        ctx: &mut LowerCtx,
        operations: &mut Vec<LoweredOp>,
    ) -> Result<()> {
        let mut roots: BTreeSet<RootSlot> = BTreeSet::new();
        for place in union {
            roots.insert(place.root);
        }
        for slot in roots {
            let root_type = self.root_slot_type(slot, param_types, ctx)?;
            let place = MovedPlace::whole(slot);
            self.emit_branch_compensation(
                root,
                &place,
                &root_type,
                &root_type,
                union,
                branch_moved,
                debug_expr_hash,
                ctx,
                operations,
            )?;
        }
        Ok(())
    }

    /// Divergence-aware (R7) two-way move merge at an `if` / desugared-scalar-`case`
    /// boundary. A divergent branch (one that always `return`s) leaves through its
    /// own `EarlyReturn`, which already dropped every value live at that point, and
    /// never reaches the merge — so it is excluded from the move union and receives
    /// no compensating drops; only the non-divergent branch(es) set `ctx.moved` for
    /// the continuation. With both branches divergent the code after is unreachable.
    /// Reduces to the plain union+compensation merge when neither diverges.
    #[allow(clippy::too_many_arguments)]
    fn merge_two_branch_moves(
        &self,
        root: &ProgramRootPayload,
        then_expr: &mut LoweredExpr,
        else_expr: &mut LoweredExpr,
        then_moved: BTreeSet<MovedPlace>,
        else_moved: BTreeSet<MovedPlace>,
        moved_before: &BTreeSet<MovedPlace>,
        param_types: &[String],
        debug_expr_hash: &str,
        ctx: &mut LowerCtx,
    ) -> Result<()> {
        let then_div = lowered_ops_diverge(&then_expr.operations);
        let else_div = lowered_ops_diverge(&else_expr.operations);
        ctx.moved = moved_before.clone();
        match (then_div, else_div) {
            (false, false) => {
                let union =
                    normalize_moved_set(then_moved.union(&else_moved).cloned().collect());
                self.emit_merge_compensation(
                    root,
                    &union,
                    &then_moved,
                    param_types,
                    debug_expr_hash,
                    ctx,
                    &mut then_expr.operations,
                )?;
                self.emit_merge_compensation(
                    root,
                    &union,
                    &else_moved,
                    param_types,
                    debug_expr_hash,
                    ctx,
                    &mut else_expr.operations,
                )?;
                for place in union {
                    ctx.mark_moved_place(place);
                }
            }
            (true, false) => {
                for place in else_moved {
                    ctx.mark_moved_place(place);
                }
            }
            (false, true) => {
                for place in then_moved {
                    ctx.mark_moved_place(place);
                }
            }
            (true, true) => {}
        }
        Ok(())
    }

    fn lower_place_value(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let lowered = self.lower_place(root, expr_hash, param_types, ctx, locals)?;
        if self.type_is_move_only(root, ctx.target_triple(), type_hash)? {
            // Move a move-only value out of a whole slot or a record-field
            // projection, recording the (possibly partial) move so the enclosing
            // aggregate's drop becomes field-granular (SPEC_V3 §7). Moving out of
            // an array element or pointer projection stays fail-closed.
            self.mark_moved_source(expr_hash, ctx, locals)?;
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "move", &id);
            let mut operations = lowered.operations;
            operations.push(LoweredOp::Move {
                id: id.clone(),
                address: lowered.address,
                type_hash: type_hash.to_string(),
            });
            return Ok(LoweredExpr {
                operations,
                value: id,
                type_hash: type_hash.to_string(),
            });
        }
        let passes_indirect = self.type_passes_indirect(root, ctx.target_triple(), type_hash)?;
        if passes_indirect {
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "copy", &id);
            let mut operations = lowered.operations;
            operations.push(LoweredOp::Copy {
                id: id.clone(),
                value: lowered.address,
                type_hash: type_hash.to_string(),
            });
            return Ok(LoweredExpr {
                operations,
                value: id,
                type_hash: type_hash.to_string(),
            });
        }
        if self.type_requires_copy_scaffold(root, ctx.target_triple(), type_hash)? {
            let loaded_id = ctx.value();
            ctx.push_debug_op(expr_hash, "load", &loaded_id);
            let copy_id = ctx.value();
            ctx.push_debug_op(expr_hash, "copy", &copy_id);
            let mut operations = lowered.operations;
            operations.push(LoweredOp::Load {
                id: loaded_id.clone(),
                address: lowered.address,
                type_hash: type_hash.to_string(),
            });
            operations.push(LoweredOp::Copy {
                id: copy_id.clone(),
                value: loaded_id,
                type_hash: type_hash.to_string(),
            });
            return Ok(LoweredExpr {
                operations,
                value: copy_id,
                type_hash: type_hash.to_string(),
            });
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "load", &id);
        let mut operations = lowered.operations;
        operations.push(LoweredOp::Load {
            id: id.clone(),
            address: lowered.address,
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_reference_value_for_place(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        match self.type_spec(type_hash)? {
            TypeSpec::Reference { .. } => {}
            _ => bail!("place reference lowering requires reference type"),
        }
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("param_ref" | "local_ref" | "field_access" | "array_index") => {
                let lowered = self.lower_place(root, expr_hash, param_types, ctx, locals)?;
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "load", &id);
                let mut operations = lowered.operations;
                operations.push(LoweredOp::Load {
                    id: id.clone(),
                    address: lowered.address,
                    type_hash: type_hash.to_string(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash: type_hash.to_string(),
                })
            }
            _ => self.lower_expr(root, expr_hash, param_types, ctx, locals),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_box_payload_place(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        box_type_hash: &str,
        element_type_hash: &str,
        debug_expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredAddress> {
        let boxed = self.lower_box_value_for_place(
            root,
            expr_hash,
            box_type_hash,
            param_types,
            ctx,
            locals,
        )?;
        let id = ctx.value();
        ctx.push_debug_op(debug_expr_hash, "deref_box", &id);
        let mut operations = boxed.operations;
        operations.push(LoweredOp::DerefBox {
            id: id.clone(),
            box_value: boxed.value,
            box_type_hash: box_type_hash.to_string(),
            element_type_hash: element_type_hash.to_string(),
        });
        Ok(LoweredAddress {
            operations,
            address: id,
            type_hash: element_type_hash.to_string(),
        })
    }

    fn lower_box_value_for_place(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        match self.type_spec(type_hash)? {
            TypeSpec::Box { .. } => {}
            _ => bail!("box place lowering requires box type"),
        }
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("param_ref" | "local_ref" | "field_access" | "array_index") => {
                let lowered = self.lower_place(root, expr_hash, param_types, ctx, locals)?;
                if lowered.type_hash != type_hash {
                    bail!("box place type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "load", &id);
                let mut operations = lowered.operations;
                operations.push(LoweredOp::Load {
                    id: id.clone(),
                    address: lowered.address,
                    type_hash: type_hash.to_string(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash: type_hash.to_string(),
                })
            }
            _ => self.lower_expr(root, expr_hash, param_types, ctx, locals),
        }
    }

    fn lower_place(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredAddress> {
        let payload = self.get_payload(expr_hash)?;
        let type_hash = expr_type(&payload, expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        match expr_kind {
            "param_ref" => {
                let slot = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                let expected = param_types
                    .get(slot)
                    .ok_or_else(|| anyhow!("parameter slot out of bounds {slot}"))?;
                if expected != &type_hash {
                    bail!("parameter slot {slot} type mismatch");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_param", &id);
                Ok(LoweredAddress {
                    operations: vec![LoweredOp::AddrOfParam {
                        id: id.clone(),
                        place: LoweredPlace::Param {
                            slot,
                            type_hash: type_hash.clone(),
                            indirect: self.type_passes_indirect(
                                root,
                                ctx.target_triple(),
                                &type_hash,
                            )?,
                        },
                    }],
                    address: id,
                    type_hash,
                })
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                let binding = local_lowered_at_depth(locals, depth)
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds {depth}"))?;
                if binding.type_hash != type_hash {
                    bail!("local_ref type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_local", &id);
                Ok(LoweredAddress {
                    operations: vec![LoweredOp::AddrOfLocal {
                        id: id.clone(),
                        place: LoweredPlace::Local {
                            slot: binding.slot,
                            type_hash: type_hash.clone(),
                        },
                    }],
                    address: id,
                    type_hash,
                })
            }
            "field_access" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                let field = payload
                    .get("field")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing field"))?;
                validate_projection_identifier("record field", field)?;
                let target_payload = self.get_payload(target_hash)?;
                let target_type = expr_type(&target_payload, target_hash)?;
                let target = match self.type_spec(&target_type)? {
                    TypeSpec::Reference {
                        mutable, referent, ..
                    } => {
                        let lowered_ref = self.lower_reference_value_for_place(
                            root,
                            target_hash,
                            &target_type,
                            param_types,
                            ctx,
                            locals,
                        )?;
                        let id = ctx.value();
                        let debug_kind = if mutable { "deref_mut" } else { "deref_shared" };
                        ctx.push_debug_op(expr_hash, debug_kind, &id);
                        let mut operations = lowered_ref.operations;
                        if mutable {
                            operations.push(LoweredOp::DerefMut {
                                id: id.clone(),
                                reference: lowered_ref.value,
                                referent_type_hash: referent.clone(),
                            });
                        } else {
                            operations.push(LoweredOp::DerefShared {
                                id: id.clone(),
                                reference: lowered_ref.value,
                                referent_type_hash: referent.clone(),
                            });
                        }
                        LoweredAddress {
                            operations,
                            address: id,
                            type_hash: referent,
                        }
                    }
                    TypeSpec::Box { element } => self.lower_box_payload_place(
                        root,
                        target_hash,
                        &target_type,
                        &element,
                        expr_hash,
                        param_types,
                        ctx,
                        locals,
                    )?,
                    _ if self.is_aggregate_ir_type(root, &target_type)?
                        && self.expr_is_place(target_hash)? =>
                    {
                        self.lower_place(root, target_hash, param_types, ctx, locals)?
                    }
                    _ if self.is_aggregate_ir_type(root, &target_type)? => {
                        let lowered_value =
                            self.lower_expr(root, target_hash, param_types, ctx, locals)?;
                        LoweredAddress {
                            operations: lowered_value.operations,
                            address: lowered_value.value,
                            type_hash: lowered_value.type_hash,
                        }
                    }
                    _ => self.lower_place(root, target_hash, param_types, ctx, locals)?,
                };
                let field_info =
                    self.lowered_record_field(root, ctx.target_triple(), &target.type_hash, field)?;
                if field_info.type_hash != type_hash {
                    bail!("field_access type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_field", &id);
                let mut operations = target.operations;
                operations.push(LoweredOp::AddrOfField {
                    id: id.clone(),
                    place: LoweredPlace::Field {
                        base: target.address,
                        field: field.to_string(),
                        field_symbol: field_info.field_symbol,
                        owner_type_hash: target.type_hash,
                        offset_bytes: field_info.offset_bytes,
                        type_hash: type_hash.clone(),
                    },
                });
                Ok(LoweredAddress {
                    operations,
                    address: id,
                    type_hash,
                })
            }
            "array_index" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let index_hash = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing index"))?;
                let target_payload = self.get_payload(target_hash)?;
                let target_type = expr_type(&target_payload, target_hash)?;
                let target = match self.type_spec(&target_type)? {
                    TypeSpec::Reference {
                        mutable, referent, ..
                    } => {
                        let lowered_ref = self.lower_reference_value_for_place(
                            root,
                            target_hash,
                            &target_type,
                            param_types,
                            ctx,
                            locals,
                        )?;
                        let id = ctx.value();
                        let debug_kind = if mutable { "deref_mut" } else { "deref_shared" };
                        ctx.push_debug_op(expr_hash, debug_kind, &id);
                        let mut operations = lowered_ref.operations;
                        if mutable {
                            operations.push(LoweredOp::DerefMut {
                                id: id.clone(),
                                reference: lowered_ref.value,
                                referent_type_hash: referent.clone(),
                            });
                        } else {
                            operations.push(LoweredOp::DerefShared {
                                id: id.clone(),
                                reference: lowered_ref.value,
                                referent_type_hash: referent.clone(),
                            });
                        }
                        LoweredAddress {
                            operations,
                            address: id,
                            type_hash: referent,
                        }
                    }
                    TypeSpec::Box { element } => self.lower_box_payload_place(
                        root,
                        target_hash,
                        &target_type,
                        &element,
                        expr_hash,
                        param_types,
                        ctx,
                        locals,
                    )?,
                    _ if self.is_aggregate_ir_type(root, &target_type)?
                        && self.expr_is_place(target_hash)? =>
                    {
                        self.lower_place(root, target_hash, param_types, ctx, locals)?
                    }
                    _ if self.is_aggregate_ir_type(root, &target_type)? => {
                        let lowered_value =
                            self.lower_expr(root, target_hash, param_types, ctx, locals)?;
                        LoweredAddress {
                            operations: lowered_value.operations,
                            address: lowered_value.value,
                            type_hash: lowered_value.type_hash,
                        }
                    }
                    _ => self.lower_place(root, target_hash, param_types, ctx, locals)?,
                };
                let index = self.lower_expr(root, index_hash, param_types, ctx, locals)?;
                if index.type_hash != type_hash_for("I64") {
                    bail!("array_index index must lower to i64");
                }
                let mut operations = target.operations;
                operations.extend(index.operations);
                let (base_address, element_type_hash) =
                    if matches!(self.type_spec(&target.type_hash)?, TypeSpec::Slice { .. }) {
                        let TypeSpec::Slice { element, .. } = self.type_spec(&target.type_hash)?
                        else {
                            unreachable!("slice type was checked");
                        };
                        if element != type_hash {
                            bail!("slice index element type mismatch while lowering");
                        }
                        let data_id = ctx.value();
                        ctx.push_debug_op(expr_hash, "slice_data", &data_id);
                        operations.push(LoweredOp::SliceData {
                            id: data_id.clone(),
                            slice: target.address.clone(),
                            slice_type_hash: target.type_hash.clone(),
                            element_type_hash: element.clone(),
                        });
                        let len_id = ctx.value();
                        ctx.push_debug_op(expr_hash, "slice_len", &len_id);
                        operations.push(LoweredOp::SliceLen {
                            id: len_id.clone(),
                            slice: target.address,
                            slice_type_hash: target.type_hash,
                            type_hash: type_hash_for("I64"),
                        });
                        let check_id = ctx.value();
                        ctx.push_debug_op(expr_hash, "bounds_check", &check_id);
                        operations.push(LoweredOp::BoundsCheck {
                            id: check_id,
                            index: index.value.clone(),
                            len: 0,
                            len_value: Some(len_id),
                            type_hash: type_hash_for("Unit"),
                        });
                        (data_id, element)
                    } else {
                        let array_info =
                            self.lowered_array_info(root, ctx.target_triple(), &target.type_hash)?;
                        if array_info.element_type_hash != type_hash {
                            bail!("array_index element type mismatch while lowering");
                        }
                        if self.literal_i64_value(index_hash)?.is_none() {
                            let check_id = ctx.value();
                            ctx.push_debug_op(expr_hash, "bounds_check", &check_id);
                            operations.push(LoweredOp::BoundsCheck {
                                id: check_id,
                                index: index.value.clone(),
                                len: array_info.len,
                                len_value: None,
                                type_hash: type_hash_for("Unit"),
                            });
                        }
                        (target.address, array_info.element_type_hash)
                    };
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_index", &id);
                operations.push(LoweredOp::AddrOfIndex {
                    id: id.clone(),
                    place: LoweredPlace::Index {
                        base: base_address,
                        index: index.value,
                        element_type_hash,
                        type_hash: type_hash.clone(),
                    },
                });
                Ok(LoweredAddress {
                    operations,
                    address: id,
                    type_hash,
                })
            }
            other => bail!("expression kind {other} is not an addressable place"),
        }
    }

    fn lower_slice_from_array(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let target_hash = payload
            .get("target")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("slice_from_array missing target"))?;
        let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
        let array_info = self.lowered_array_info(root, ctx.target_triple(), &target.type_hash)?;
        let TypeSpec::Slice {
            element,
            mutable: _,
            ..
        } = self.type_spec(type_hash)?
        else {
            bail!("slice_from_array result must be slice");
        };
        if element != array_info.element_type_hash {
            bail!("slice_from_array element type mismatch while lowering");
        }

        let len_id = ctx.value();
        ctx.push_debug_op(expr_hash, "const_i64", &len_id);
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), type_hash)?);
        let slot = ctx.local_slot(type_hash.to_string(), slot_size);
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "construct_slice", &id);
        let mut operations = target.operations;
        operations.push(LoweredOp::ConstI64 {
            id: len_id.clone(),
            value: array_info.len.to_string(),
            type_hash: type_hash_for("I64"),
        });
        operations.push(LoweredOp::AddrOfLocal {
            id: address.clone(),
            place: LoweredPlace::Local {
                slot,
                type_hash: type_hash.to_string(),
            },
        });
        operations.push(LoweredOp::ConstructSlice {
            id: id.clone(),
            address: address.clone(),
            data_address: target.address,
            len: len_id,
            element_type_hash: array_info.element_type_hash,
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_static_bytes(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        ctx: &mut LowerCtx,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let static_data_hash = payload
            .get("static_data")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("static_bytes missing static_data"))?
            .to_string();
        let bytes_hex = self.static_data_bytes_hex(&static_data_hash)?;
        let len = u64::try_from(bytes_hex.len() / 2)?;
        if payload.get("bytes_len").and_then(JsonValue::as_u64) != Some(len) {
            bail!("static_bytes bytes_len mismatch while lowering");
        }
        let element_type_hash = payload
            .get("element_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("static_bytes missing element_type"))?
            .to_string();
        if element_type_hash != type_hash_for("U8") {
            bail!("static_bytes element_type must be u8 while lowering");
        }
        match self.type_spec(type_hash)? {
            TypeSpec::Slice {
                region,
                mutable: false,
                element,
            } if is_static_region(&region) && element == element_type_hash => {}
            _ => bail!("static_bytes result must be slice<'static, u8>"),
        }

        let data_id = ctx.value();
        ctx.push_debug_op(expr_hash, "static_data_address", &data_id);
        let len_id = ctx.value();
        ctx.push_debug_op(expr_hash, "const_i64", &len_id);
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), type_hash)?);
        let slot = ctx.local_slot(type_hash.to_string(), slot_size);
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "construct_slice", &id);
        let operations = vec![
            LoweredOp::StaticDataAddress {
                id: data_id.clone(),
                static_data_hash,
                bytes_hex,
                len,
                element_type_hash: element_type_hash.clone(),
            },
            LoweredOp::ConstI64 {
                id: len_id.clone(),
                value: len.to_string(),
                type_hash: type_hash_for("I64"),
            },
            LoweredOp::AddrOfLocal {
                id: address.clone(),
                place: LoweredPlace::Local {
                    slot,
                    type_hash: type_hash.to_string(),
                },
            },
            LoweredOp::ConstructSlice {
                id: id.clone(),
                address,
                data_address: data_id,
                len: len_id,
                element_type_hash,
                type_hash: type_hash.to_string(),
            },
        ];
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_box_new(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        self.lower_box_new_as(root, expr_hash, type_hash, param_types, ctx, locals)
    }

    fn lower_box_new_as(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        result_box_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("box_new missing value"))?;
        let declared_element_type = payload
            .get("element_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("box_new missing element_type"))?
            .to_string();
        let element_type_hash = match self.type_spec(result_box_type)? {
            TypeSpec::Box { element } => element,
            _ => bail!("box_new lowered type mismatch"),
        };
        if !self.type_assignable_in_root(root, &declared_element_type, &element_type_hash)? {
            bail!("box_new value type mismatch while lowering");
        }
        let element_layout =
            self.compute_type_layout(root, &element_type_hash, ctx.target_triple())?;
        let size_bytes = element_layout
            .metadata
            .get("size_bytes")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("box_new element layout missing size"))?;
        let align_bytes = element_layout
            .metadata
            .get("align_bytes")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("box_new element layout missing align"))?;
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "heap_alloc", &id);
        let mut operations = vec![LoweredOp::HeapAlloc {
            id: id.clone(),
            size_bytes,
            align_bytes,
            element_type_hash: element_type_hash.clone(),
            type_hash: result_box_type.to_string(),
        }];
        let value = self.lower_expr_as(
            root,
            value_hash,
            &element_type_hash,
            param_types,
            ctx,
            locals,
        )?;
        if !self.type_assignable_in_root(root, &value.type_hash, &element_type_hash)? {
            bail!("box_new value type mismatch while lowering");
        }
        operations.extend(value.operations);
        operations.push(LoweredOp::Store {
            address: id.clone(),
            value: value.value,
            type_hash: element_type_hash,
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: result_box_type.to_string(),
        })
    }

    /// Lower `unbox(b)`: move the payload out of `b: box<T>` and free the shell.
    /// The box argument is lowered as a consumed value (a move-only `box` place is
    /// `Move`d, marking its slot moved so it is not also dropped at scope exit), and
    /// the payload is copied into an owned scratch slot before the shell is freed.
    fn lower_unbox(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        result_element_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("unbox missing value"))?;
        let box_type_hash = payload
            .get("box_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("unbox missing box_type"))?
            .to_string();
        let element_type_hash = match self.type_spec(&box_type_hash)? {
            TypeSpec::Box { element } => element,
            _ => bail!("unbox lowered type mismatch"),
        };
        if element_type_hash != result_element_type {
            bail!("unbox lowered element type mismatch");
        }
        let boxed =
            self.lower_expr_as(root, value_hash, &box_type_hash, param_types, ctx, locals)?;
        if boxed.type_hash != box_type_hash {
            bail!("unbox box value type mismatch while lowering");
        }
        // Owned scratch slot for the moved-out payload. Like a record/enum literal's
        // backing slot, it is NOT pushed as a droppable local: ownership of the
        // payload flows out through the result value, which the consumer copies.
        self.compute_type_layout(root, &element_type_hash, ctx.target_triple())?;
        let slot_size = stack_slot_size_bytes(self.layout_size_bytes(
            root,
            ctx.target_triple(),
            &element_type_hash,
        )?);
        let dest_slot = ctx.local_slot(element_type_hash.clone(), slot_size);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "unbox_move", &id);
        let mut operations = boxed.operations;
        operations.push(LoweredOp::UnboxMove {
            id: id.clone(),
            box_value: boxed.value,
            box_type_hash,
            element_type_hash: element_type_hash.clone(),
            dest_slot,
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: element_type_hash,
        })
    }

    fn lower_vec_new(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        _param_types: &[String],
        ctx: &mut LowerCtx,
        _locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let element_type_hash = payload
            .get("element_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("vec_new missing element_type"))?
            .to_string();
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Vec { element } if element == element_type_hash => {}
            _ => bail!("vec_new lowered type mismatch"),
        }
        let capacity = payload
            .get("capacity_value")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("vec_new missing capacity_value"))?;
        self.compute_type_layout(root, &element_type_hash, ctx.target_triple())?;
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), type_hash)?);
        let slot = ctx.local_slot(type_hash.to_string(), slot_size);
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "vec_new", &id);
        Ok(LoweredExpr {
            operations: vec![
                LoweredOp::AddrOfLocal {
                    id: address.clone(),
                    place: LoweredPlace::Local {
                        slot,
                        type_hash: type_hash.to_string(),
                    },
                },
                LoweredOp::VecNew {
                    id: id.clone(),
                    address,
                    capacity,
                    element_type_hash,
                    type_hash: type_hash.to_string(),
                },
            ],
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_vec_push(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let target_hash = payload
            .get("target")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("vec_push missing target"))?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("vec_push missing value"))?;
        let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
        let TypeSpec::Vec { element } = self.type_spec_in_root(root, &target.type_hash)? else {
            bail!("vec_push target must lower as vec<T>");
        };
        let declared_element = payload
            .get("element_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("vec_push missing element_type"))?;
        if declared_element != element {
            bail!("vec_push element_type mismatch while lowering");
        }
        let value = self.lower_expr_as(root, value_hash, &element, param_types, ctx, locals)?;
        if !self.type_assignable_in_root(root, &value.type_hash, &element)? {
            bail!("vec_push value type mismatch while lowering");
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "vec_push", &id);
        let mut operations = target.operations;
        operations.extend(value.operations);
        operations.push(LoweredOp::VecPush {
            id: id.clone(),
            vec_address: target.address,
            value: value.value,
            vec_type_hash: target.type_hash,
            element_type_hash: element,
            type_hash: type_hash_for("Unit"),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash_for("Unit"),
        })
    }

    fn lower_vec_get(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let target_hash = payload
            .get("target")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("vec_get missing target"))?;
        let index_hash = payload
            .get("index")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("vec_get missing index"))?;
        let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
        let TypeSpec::Vec { element } = self.type_spec_in_root(root, &target.type_hash)? else {
            bail!("vec_get target must lower as vec<T>");
        };
        if element != type_hash {
            bail!("vec_get result type mismatch while lowering");
        }
        let index = self.lower_expr(root, index_hash, param_types, ctx, locals)?;
        if index.type_hash != type_hash_for("I64") {
            bail!("vec_get index type mismatch while lowering");
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "vec_get", &id);
        let mut operations = target.operations;
        operations.extend(index.operations);
        operations.push(LoweredOp::VecGet {
            id: id.clone(),
            vec_address: target.address,
            index: index.value,
            vec_type_hash: target.type_hash,
            element_type_hash: element,
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_vec_len(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let target_hash = payload
            .get("target")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("vec_len missing target"))?;
        let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
        if !matches!(
            self.type_spec_in_root(root, &target.type_hash)?,
            TypeSpec::Vec { .. }
        ) {
            bail!("vec_len target must lower as vec<T>");
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "vec_len", &id);
        let mut operations = target.operations;
        operations.push(LoweredOp::VecLen {
            id: id.clone(),
            vec_address: target.address,
            vec_type_hash: target.type_hash,
            type_hash: type_hash_for("I64"),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash_for("I64"),
        })
    }

    fn lower_string_new(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        ctx: &mut LowerCtx,
    ) -> Result<LoweredExpr> {
        if !matches!(self.type_spec_in_root(root, type_hash)?, TypeSpec::String) {
            bail!("string_new lowered type mismatch");
        }
        let payload = self.get_payload(expr_hash)?;
        let static_data_hash = payload
            .get("source_static_data")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("string_new missing source_static_data"))?
            .to_string();
        let bytes_hex = self.static_data_bytes_hex(&static_data_hash)?;
        let len = u64::try_from(bytes_hex.len() / 2)?;
        if payload.get("bytes_len").and_then(JsonValue::as_u64) != Some(len) {
            bail!("string_new bytes_len mismatch while lowering");
        }
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), type_hash)?);
        let slot = ctx.local_slot(type_hash.to_string(), slot_size);
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "string_new", &id);
        Ok(LoweredExpr {
            operations: vec![
                LoweredOp::AddrOfLocal {
                    id: address.clone(),
                    place: LoweredPlace::Local {
                        slot,
                        type_hash: type_hash.to_string(),
                    },
                },
                LoweredOp::StringNew {
                    id: id.clone(),
                    address,
                    static_data_hash,
                    bytes_hex,
                    len,
                    type_hash: type_hash.to_string(),
                },
            ],
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_string_len(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let target_hash = payload
            .get("target")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("string_len missing target"))?;
        let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
        if !matches!(
            self.type_spec_in_root(root, &target.type_hash)?,
            TypeSpec::String
        ) {
            bail!("string_len target must lower as string");
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "string_len", &id);
        let mut operations = target.operations;
        operations.push(LoweredOp::StringLen {
            id: id.clone(),
            string_address: target.address,
            string_type_hash: target.type_hash,
            type_hash: type_hash_for("I64"),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash_for("I64"),
        })
    }

    fn lower_string_with_capacity(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        if !matches!(self.type_spec_in_root(root, type_hash)?, TypeSpec::String) {
            bail!("string_with_capacity lowered type mismatch");
        }
        let payload = self.get_payload(expr_hash)?;
        let capacity_hash = payload
            .get("capacity")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("string_with_capacity missing capacity"))?;
        let capacity = self.lower_expr(root, capacity_hash, param_types, ctx, locals)?;
        if capacity.type_hash != type_hash_for("I64") {
            bail!("string_with_capacity capacity must lower as i64");
        }
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), type_hash)?);
        let slot = ctx.local_slot(type_hash.to_string(), slot_size);
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "string_with_capacity", &id);
        let mut operations = capacity.operations;
        operations.push(LoweredOp::AddrOfLocal {
            id: address.clone(),
            place: LoweredPlace::Local {
                slot,
                type_hash: type_hash.to_string(),
            },
        });
        operations.push(LoweredOp::StringWithCapacity {
            id: id.clone(),
            address,
            capacity: capacity.value,
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_string_push(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let target_hash = payload
            .get("target")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("string_push missing target"))?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("string_push missing value"))?;
        let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
        if !matches!(
            self.type_spec_in_root(root, &target.type_hash)?,
            TypeSpec::String
        ) {
            bail!("string_push target must lower as string");
        }
        let value = self.lower_expr_as(
            root,
            value_hash,
            &type_hash_for("U8"),
            param_types,
            ctx,
            locals,
        )?;
        if value.type_hash != type_hash_for("U8") {
            bail!("string_push value must lower as u8");
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "string_push", &id);
        let mut operations = target.operations;
        operations.extend(value.operations);
        operations.push(LoweredOp::StringPush {
            id: id.clone(),
            string_address: target.address,
            value: value.value,
            string_type_hash: target.type_hash,
            type_hash: type_hash_for("Unit"),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash_for("Unit"),
        })
    }

    fn lower_string_get(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        if type_hash != type_hash_for("U8") {
            bail!("string_get lowered result must be u8");
        }
        let payload = self.get_payload(expr_hash)?;
        let target_hash = payload
            .get("target")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("string_get missing target"))?;
        let index_hash = payload
            .get("index")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("string_get missing index"))?;
        let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
        if !matches!(
            self.type_spec_in_root(root, &target.type_hash)?,
            TypeSpec::String
        ) {
            bail!("string_get target must lower as string");
        }
        let index = self.lower_expr(root, index_hash, param_types, ctx, locals)?;
        if index.type_hash != type_hash_for("I64") {
            bail!("string_get index type mismatch while lowering");
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "string_get", &id);
        let mut operations = target.operations;
        operations.extend(index.operations);
        operations.push(LoweredOp::StringGet {
            id: id.clone(),
            string_address: target.address,
            index: index.value,
            string_type_hash: target.type_hash,
            type_hash: type_hash_for("U8"),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash_for("U8"),
        })
    }

    fn lower_raw_ptr_cast(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?;
        let source_type_hash = payload
            .get("source_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_ptr_cast missing source_type"))?
            .to_string();
        let pointee_type_hash = payload
            .get("pointee_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_ptr_cast missing pointee_type"))?;
        let mutable = payload
            .get("mutable")
            .and_then(JsonValue::as_bool)
            .ok_or_else(|| anyhow!("raw_ptr_cast missing mutable"))?;
        match self.type_spec(type_hash)? {
            TypeSpec::RawPointer {
                mutable: m,
                pointee,
            } if m == mutable && pointee == pointee_type_hash => {}
            _ => bail!("raw_ptr_cast result type mismatch while lowering"),
        }
        let value = self.lower_expr_as(
            root,
            value_hash,
            &source_type_hash,
            param_types,
            ctx,
            locals,
        )?;
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "ptr_cast", &id);
        let mut operations = value.operations;
        operations.push(LoweredOp::PtrCast {
            id: id.clone(),
            value: value.value,
            source_type_hash,
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_raw_load(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let pointer_hash = payload
            .get("pointer")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_load missing pointer"))?;
        let pointer_type_hash = payload
            .get("pointer_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_load missing pointer_type"))?
            .to_string();
        let pointee_type_hash = payload
            .get("pointee_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_load missing pointee_type"))?
            .to_string();
        let TypeSpec::RawPointer { pointee, .. } = self.type_spec(&pointer_type_hash)? else {
            bail!("raw_load pointer_type is not raw pointer");
        };
        if pointee != pointee_type_hash {
            bail!("raw_load pointee metadata mismatch while lowering");
        }
        let pointer = self.lower_expr_as(
            root,
            pointer_hash,
            &pointer_type_hash,
            param_types,
            ctx,
            locals,
        )?;
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "deref_raw", &address);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "load", &id);
        let mut operations = pointer.operations;
        operations.push(LoweredOp::DerefRaw {
            id: address.clone(),
            pointer: pointer.value,
            pointer_type_hash,
            pointee_type_hash: pointee_type_hash.clone(),
            mutable: false,
        });
        operations.push(LoweredOp::Load {
            id: id.clone(),
            address,
            type_hash: pointee_type_hash.clone(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: pointee_type_hash,
        })
    }

    fn lower_raw_store(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let pointer_hash = payload
            .get("pointer")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_store missing pointer"))?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_store missing value"))?;
        let pointer_type_hash = payload
            .get("pointer_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_store missing pointer_type"))?
            .to_string();
        let pointee_type_hash = payload
            .get("pointee_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("raw_store missing pointee_type"))?
            .to_string();
        let TypeSpec::RawPointer {
            mutable: true,
            pointee,
        } = self.type_spec(&pointer_type_hash)?
        else {
            bail!("raw_store pointer_type is not raw mutable pointer");
        };
        if pointee != pointee_type_hash {
            bail!("raw_store pointee metadata mismatch while lowering");
        }
        let pointer = self.lower_expr_as(
            root,
            pointer_hash,
            &pointer_type_hash,
            param_types,
            ctx,
            locals,
        )?;
        let value = self.lower_expr_as(
            root,
            value_hash,
            &pointee_type_hash,
            param_types,
            ctx,
            locals,
        )?;
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "deref_raw", &address);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "const_unit", &id);
        let mut operations = pointer.operations;
        operations.extend(value.operations);
        operations.push(LoweredOp::DerefRaw {
            id: address.clone(),
            pointer: pointer.value,
            pointer_type_hash,
            pointee_type_hash: pointee_type_hash.clone(),
            mutable: true,
        });
        operations.push(LoweredOp::Store {
            address,
            value: value.value,
            type_hash: pointee_type_hash,
        });
        operations.push(LoweredOp::ConstUnit {
            id: id.clone(),
            type_hash: type_hash_for("Unit"),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash_for("Unit"),
        })
    }

    fn lower_slice_len(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let target_hash = payload
            .get("target")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("slice_len missing target"))?;
        let target =
            self.lower_slice_address_for_expr(root, target_hash, param_types, ctx, locals)?;
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "slice_len", &id);
        let mut operations = target.operations;
        operations.push(LoweredOp::SliceLen {
            id: id.clone(),
            slice: target.address,
            slice_type_hash: target.type_hash,
            type_hash: type_hash_for("I64"),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash_for("I64"),
        })
    }

    fn lower_subslice(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
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
        let TypeSpec::Slice { element, .. } = self.type_spec(type_hash)? else {
            bail!("subslice result must be slice");
        };
        let target =
            self.lower_slice_address_for_expr(root, target_hash, param_types, ctx, locals)?;
        if target.type_hash != type_hash {
            bail!("subslice target/result type mismatch while lowering");
        }
        let start = self.lower_expr(root, start_hash, param_types, ctx, locals)?;
        if start.type_hash != type_hash_for("I64") {
            bail!("subslice start must lower to i64");
        }
        let len = self.lower_expr(root, len_hash, param_types, ctx, locals)?;
        if len.type_hash != type_hash_for("I64") {
            bail!("subslice len must lower to i64");
        }
        let data_id = ctx.value();
        ctx.push_debug_op(expr_hash, "slice_data", &data_id);
        let orig_len_id = ctx.value();
        ctx.push_debug_op(expr_hash, "slice_len", &orig_len_id);
        let check_id = ctx.value();
        ctx.push_debug_op(expr_hash, "slice_range_check", &check_id);
        let element_addr = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_index", &element_addr);
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), type_hash)?);
        let slot = ctx.local_slot(type_hash.to_string(), slot_size);
        let slice_addr = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &slice_addr);
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "construct_slice", &id);
        let mut operations = target.operations;
        operations.extend(start.operations);
        operations.extend(len.operations);
        operations.push(LoweredOp::SliceData {
            id: data_id.clone(),
            slice: target.address.clone(),
            slice_type_hash: target.type_hash.clone(),
            element_type_hash: element.clone(),
        });
        operations.push(LoweredOp::SliceLen {
            id: orig_len_id.clone(),
            slice: target.address,
            slice_type_hash: target.type_hash,
            type_hash: type_hash_for("I64"),
        });
        operations.push(LoweredOp::SliceRangeCheck {
            id: check_id,
            start: start.value.clone(),
            len: len.value.clone(),
            source_len: orig_len_id,
            type_hash: type_hash_for("Unit"),
        });
        operations.push(LoweredOp::AddrOfIndex {
            id: element_addr.clone(),
            place: LoweredPlace::Index {
                base: data_id,
                index: start.value,
                element_type_hash: element.clone(),
                type_hash: element,
            },
        });
        operations.push(LoweredOp::AddrOfLocal {
            id: slice_addr.clone(),
            place: LoweredPlace::Local {
                slot,
                type_hash: type_hash.to_string(),
            },
        });
        operations.push(LoweredOp::ConstructSlice {
            id: id.clone(),
            address: slice_addr,
            data_address: element_addr,
            len: len.value,
            element_type_hash: payload
                .get("element_type")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("subslice missing element_type"))?
                .to_string(),
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_slice_address_for_expr(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredAddress> {
        let type_hash = self.expr_declared_type(expr_hash)?;
        if !matches!(self.type_spec(&type_hash)?, TypeSpec::Slice { .. }) {
            bail!("slice operation target must be slice");
        }
        if self.expr_is_place(expr_hash)? {
            return self.lower_place(root, expr_hash, param_types, ctx, locals);
        }
        let lowered = self.lower_expr(root, expr_hash, param_types, ctx, locals)?;
        if lowered.type_hash != type_hash {
            bail!("slice target type mismatch while lowering");
        }
        if !self.type_passes_indirect(root, ctx.target_triple(), &type_hash)? {
            bail!("slice values must lower indirectly");
        }
        Ok(LoweredAddress {
            operations: lowered.operations,
            address: lowered.value,
            type_hash,
        })
    }

    fn lower_fold(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
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
        let element_type_hash = payload
            .get("element_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("fold missing element_type"))?
            .to_string();
        let acc_type_hash = payload
            .get("acc_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("fold missing acc_type"))?
            .to_string();
        if acc_type_hash != type_hash {
            bail!("fold accumulator/result type mismatch while lowering");
        }
        if self.type_is_move_only(root, ctx.target_triple(), &element_type_hash)?
            || self.type_is_move_only(root, ctx.target_triple(), &acc_type_hash)?
        {
            bail!("fold lowering requires copyable element and accumulator types in phase 13");
        }

        let mut operations;
        let (target_address, target_type_hash, len_value) = match payload
            .get("target_kind")
            .and_then(JsonValue::as_str)
        {
            Some("fixed_array") => {
                let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
                let array_info =
                    self.lowered_array_info(root, ctx.target_triple(), &target.type_hash)?;
                if array_info.element_type_hash != element_type_hash {
                    bail!("fold array element type mismatch while lowering");
                }
                let len_id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_i64", &len_id);
                operations = target.operations;
                operations.push(LoweredOp::ConstI64 {
                    id: len_id.clone(),
                    value: array_info.len.to_string(),
                    type_hash: type_hash_for("I64"),
                });
                (target.address, target.type_hash, len_id)
            }
            Some("slice") => {
                let target =
                    self.lower_slice_address_for_expr(root, target_hash, param_types, ctx, locals)?;
                match self.type_spec(&target.type_hash)? {
                    TypeSpec::Slice { element, .. } if element == element_type_hash => {}
                    _ => bail!("fold slice element type mismatch while lowering"),
                }
                let len_id = ctx.value();
                ctx.push_debug_op(expr_hash, "slice_len", &len_id);
                operations = target.operations;
                operations.push(LoweredOp::SliceLen {
                    id: len_id.clone(),
                    slice: target.address.clone(),
                    slice_type_hash: target.type_hash.clone(),
                    type_hash: type_hash_for("I64"),
                });
                (target.address, target.type_hash, len_id)
            }
            Some(other) => bail!("unknown fold target_kind {other}"),
            None => bail!("fold missing target_kind"),
        };

        let init = self.lower_expr_as(root, init_hash, &acc_type_hash, param_types, ctx, locals)?;
        if !self.type_assignable_in_root(root, &init.type_hash, &acc_type_hash)? {
            bail!("fold init type mismatch while lowering");
        }
        operations.extend(init.operations);

        let index_slot_size = stack_slot_size_bytes(self.layout_size_bytes(
            root,
            ctx.target_triple(),
            &type_hash_for("I64"),
        )?);
        let index_slot = ctx.local_slot(type_hash_for("I64"), index_slot_size);
        let item_slot_size = stack_slot_size_bytes(self.layout_size_bytes(
            root,
            ctx.target_triple(),
            &element_type_hash,
        )?);
        let item_slot = ctx.local_slot(element_type_hash.clone(), item_slot_size);
        let acc_slot_size = stack_slot_size_bytes(self.layout_size_bytes(
            root,
            ctx.target_triple(),
            &acc_type_hash,
        )?);
        let acc_slot = ctx.local_slot(acc_type_hash.clone(), acc_slot_size);

        locals.push(LocalLoweredBinding {
            slot: item_slot,
            type_hash: element_type_hash.clone(),
        });
        locals.push(LocalLoweredBinding {
            slot: acc_slot,
            type_hash: acc_type_hash.clone(),
        });
        let moved_before = ctx.moved.clone();
        let body = self.lower_expr_as(root, body_hash, &acc_type_hash, param_types, ctx, locals);
        locals.pop();
        locals.pop();
        let body = body?;
        if ctx.moved != moved_before {
            bail!(
                "unsupported_move: fold body cannot move owned values in phase 13; loop-aware drop glue is not yet implemented"
            );
        }
        if !self.type_assignable_in_root(root, &body.type_hash, &acc_type_hash)? {
            bail!("fold body type mismatch while lowering");
        }

        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "fold", &id);
        operations.push(LoweredOp::Fold {
            id: id.clone(),
            target_address,
            target_type_hash,
            len: len_value,
            init: init.value,
            index_slot,
            acc_slot,
            item_slot,
            body: LoweredBlock {
                operations: body.operations,
                result: body.value,
            },
            element_type_hash,
            acc_type_hash: acc_type_hash.clone(),
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    /// Lower `loop acc = init while cond do body` (R8): the condition-driven
    /// counterpart of `lower_fold`. `init` lowers and seeds the accumulator slot;
    /// `cond` and `body` lower as blocks that read the accumulator slot (via the
    /// `acc` local binding). Like fold, the accumulator must be copyable and the
    /// body may move no owned values (loop-carried drop glue is a follow-on;
    /// SPEC_V3 §7) — a body move is rejected here.
    fn lower_loop(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
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
        let acc_type_hash = payload
            .get("acc_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("loop missing acc_type"))?
            .to_string();
        if acc_type_hash != type_hash {
            bail!("loop accumulator/result type mismatch while lowering");
        }
        if self.type_is_move_only(root, ctx.target_triple(), &acc_type_hash)? {
            bail!("loop lowering requires a copyable accumulator type");
        }
        let init = self.lower_expr_as(root, init_hash, &acc_type_hash, param_types, ctx, locals)?;
        if !self.type_assignable_in_root(root, &init.type_hash, &acc_type_hash)? {
            bail!("loop init type mismatch while lowering");
        }
        let mut operations = init.operations;
        let acc_slot_size = stack_slot_size_bytes(self.layout_size_bytes(
            root,
            ctx.target_triple(),
            &acc_type_hash,
        )?);
        let acc_slot = ctx.local_slot(acc_type_hash.clone(), acc_slot_size);
        locals.push(LocalLoweredBinding {
            slot: acc_slot,
            type_hash: acc_type_hash.clone(),
        });
        let moved_before = ctx.moved.clone();
        // `cond` and `body` both read the accumulator slot via the `acc` local.
        let cond = self.lower_expr(root, cond_hash, param_types, ctx, locals);
        let body = self.lower_expr_as(root, body_hash, &acc_type_hash, param_types, ctx, locals);
        locals.pop();
        let cond = cond?;
        let body = body?;
        if ctx.moved != moved_before {
            bail!(
                "unsupported_move: loop body cannot move owned values; loop-carried drop glue is not yet implemented"
            );
        }
        if cond.type_hash != type_hash_for("Bool") {
            bail!("loop condition must lower to bool");
        }
        if !self.type_assignable_in_root(root, &body.type_hash, &acc_type_hash)? {
            bail!("loop body type mismatch while lowering");
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "loop", &id);
        operations.push(LoweredOp::Loop {
            id: id.clone(),
            acc_slot,
            init: init.value,
            cond: LoweredBlock {
                operations: cond.operations,
                result: cond.value,
            },
            body: LoweredBlock {
                operations: body.operations,
                result: body.value,
            },
            acc_type_hash: acc_type_hash.clone(),
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_record_init_to_address(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        target_type: &str,
        target_address: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<Vec<LoweredOp>> {
        let payload = self.get_payload(expr_hash)?;
        if payload.get("expr_kind").and_then(JsonValue::as_str) != Some("record_literal") {
            bail!("record initializer must be record_literal");
        }
        let mut operations = Vec::new();
        for field in payload
            .get("fields")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("record_literal missing fields"))?
        {
            let name = field
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("record field missing name"))?;
            let value_hash = field
                .get("value")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("record field missing value"))?;
            let field_info =
                self.lowered_record_field(root, ctx.target_triple(), target_type, name)?;
            let value = self.lower_expr_as(
                root,
                value_hash,
                &field_info.type_hash,
                param_types,
                ctx,
                locals,
            )?;
            if !self.type_assignable_in_root(root, &value.type_hash, &field_info.type_hash)? {
                bail!("record initializer field {name} type mismatch while lowering");
            }
            let store_type_hash = if value.type_hash == field_info.type_hash {
                field_info.type_hash.clone()
            } else {
                value.type_hash.clone()
            };
            operations.extend(value.operations);
            let field_address = ctx.value();
            ctx.push_debug_op(expr_hash, "addr_of_field", &field_address);
            operations.push(LoweredOp::AddrOfField {
                id: field_address.clone(),
                place: LoweredPlace::Field {
                    base: target_address.to_string(),
                    field: name.to_string(),
                    field_symbol: field_info.field_symbol,
                    owner_type_hash: target_type.to_string(),
                    offset_bytes: field_info.offset_bytes,
                    type_hash: store_type_hash.clone(),
                },
            });
            operations.push(LoweredOp::Store {
                address: field_address,
                value: value.value,
                type_hash: store_type_hash,
            });
        }
        Ok(operations)
    }

    /// Lower an expression's value into a destination address, picking the
    /// in-place initializer strategy by the value's kind (record/enum/array
    /// aggregates initialize their destination field-by-field or slot-by-slot; a
    /// place-typed aggregate is whole-slot copied/moved; everything else lowers as
    /// a value and is stored). Shared by the `let`-binding path and `array_set`'s
    /// source-array copy, so both write a value into a slot identically.
    #[allow(clippy::too_many_arguments)]
    fn lower_value_into_address(
        &self,
        root: &ProgramRootPayload,
        value_hash: &str,
        target_type: &str,
        target_address: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<Vec<LoweredOp>> {
        let value_kind = self
            .get_payload(value_hash)?
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        let mut operations = Vec::new();
        if value_kind.as_deref() == Some("record_literal") {
            operations.extend(self.lower_record_init_to_address(
                root,
                value_hash,
                target_type,
                target_address,
                param_types,
                ctx,
                locals,
            )?);
        } else if value_kind.as_deref() == Some("enum_construct") {
            operations.extend(self.lower_enum_init_to_address(
                root,
                value_hash,
                target_type,
                target_address,
                param_types,
                ctx,
                locals,
            )?);
        } else if matches!(
            value_kind.as_deref(),
            Some("array_literal") | Some("array_fill") | Some("array_set")
        ) {
            operations.extend(self.lower_array_init_to_address(
                root,
                value_hash,
                target_type,
                target_address,
                param_types,
                ctx,
                locals,
            )?);
        } else if self.type_passes_indirect(root, ctx.target_triple(), target_type)?
            && self.expr_is_place(value_hash)?
        {
            operations.extend(self.lower_aggregate_place_init_to_address(
                root,
                value_hash,
                target_type,
                target_address,
                param_types,
                ctx,
                locals,
            )?);
        } else {
            let value =
                self.lower_expr_as(root, value_hash, target_type, param_types, ctx, locals)?;
            if !self.type_assignable_in_root(root, &value.type_hash, target_type)? {
                bail!("let binding type mismatch while lowering");
            }
            operations.extend(value.operations);
            operations.push(LoweredOp::Store {
                address: target_address.to_string(),
                value: value.value,
                type_hash: target_type.to_string(),
            });
        }
        Ok(operations)
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_array_init_to_address(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        target_type: &str,
        target_address: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<Vec<LoweredOp>> {
        let payload = self.get_payload(expr_hash)?;
        let array_info = self.lowered_array_info(root, ctx.target_triple(), target_type)?;
        let kind = payload.get("expr_kind").and_then(JsonValue::as_str);
        // `array_set(arr, i, v)` (R9): a functional array update. Initialize the
        // destination with a copy of the source array `arr` (the whole Copy slot),
        // then overwrite element `i` with `v` — a bounds-checked indexed store. The
        // element type rule (non-reference Copy, trivial drop) makes the source copy
        // a blind whole-slot copy and the overwrite leak-free.
        if kind == Some("array_set") {
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
            // 1. Copy the source array into the destination slot.
            let mut operations = self.lower_value_into_address(
                root,
                array_hash,
                target_type,
                target_address,
                param_types,
                ctx,
                locals,
            )?;
            // 2. Lower the index and bounds-check it against the array length.
            let index = self.lower_expr(root, index_hash, param_types, ctx, locals)?;
            if index.type_hash != type_hash_for("I64") {
                bail!("array_set index must lower to i64");
            }
            operations.extend(index.operations);
            if self.literal_i64_value(index_hash)?.is_none() {
                let check_id = ctx.value();
                ctx.push_debug_op(expr_hash, "bounds_check", &check_id);
                operations.push(LoweredOp::BoundsCheck {
                    id: check_id,
                    index: index.value.clone(),
                    len: array_info.len,
                    len_value: None,
                    type_hash: type_hash_for("Unit"),
                });
            }
            // 3. Overwrite element `i` with the (Copy) value.
            let value = self.lower_expr_as(
                root,
                value_hash,
                &array_info.element_type_hash,
                param_types,
                ctx,
                locals,
            )?;
            if !self.type_assignable_in_root(root, &value.type_hash, &array_info.element_type_hash)?
            {
                bail!("array_set value type mismatch while lowering");
            }
            operations.extend(value.operations);
            let element_address = ctx.value();
            ctx.push_debug_op(expr_hash, "addr_of_index", &element_address);
            operations.push(LoweredOp::AddrOfIndex {
                id: element_address.clone(),
                place: LoweredPlace::Index {
                    base: target_address.to_string(),
                    index: index.value,
                    element_type_hash: array_info.element_type_hash.clone(),
                    type_hash: array_info.element_type_hash.clone(),
                },
            });
            operations.push(LoweredOp::Store {
                address: element_address,
                value: value.value,
                type_hash: array_info.element_type_hash.clone(),
            });
            return Ok(operations);
        }
        // `[value; count]` (R9): lower `value` ONCE, then store the (Copy) result into
        // every slot. The single lowered value is reused by all stores — the type
        // rule guarantees a non-reference Copy value, so replicating it is sound.
        if kind == Some("array_fill") {
            let value_hash = payload
                .get("value")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("array_fill missing value"))?;
            let value = self.lower_expr_as(
                root,
                value_hash,
                &array_info.element_type_hash,
                param_types,
                ctx,
                locals,
            )?;
            if !self.type_assignable_in_root(root, &value.type_hash, &array_info.element_type_hash)?
            {
                bail!("array fill value type mismatch while lowering");
            }
            let mut operations = value.operations;
            for idx in 0..array_info.len {
                let index_id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_i64", &index_id);
                operations.push(LoweredOp::ConstI64 {
                    id: index_id.clone(),
                    value: idx.to_string(),
                    type_hash: type_hash_for("I64"),
                });
                let element_address = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_index", &element_address);
                operations.push(LoweredOp::AddrOfIndex {
                    id: element_address.clone(),
                    place: LoweredPlace::Index {
                        base: target_address.to_string(),
                        index: index_id,
                        element_type_hash: array_info.element_type_hash.clone(),
                        type_hash: array_info.element_type_hash.clone(),
                    },
                });
                operations.push(LoweredOp::Store {
                    address: element_address,
                    value: value.value.clone(),
                    type_hash: array_info.element_type_hash.clone(),
                });
            }
            return Ok(operations);
        }
        if kind != Some("array_literal") {
            bail!("array initializer must be array_literal or array_fill");
        }
        let elements = payload
            .get("elements")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("array_literal missing elements"))?;
        if elements.len() as u64 != array_info.len {
            bail!("array initializer length mismatch while lowering");
        }
        let mut operations = Vec::new();
        for (idx, element) in elements.iter().enumerate() {
            let value_hash = element
                .get("value")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("array element missing value"))?;
            let value = self.lower_expr_as(
                root,
                value_hash,
                &array_info.element_type_hash,
                param_types,
                ctx,
                locals,
            )?;
            if !self.type_assignable_in_root(
                root,
                &value.type_hash,
                &array_info.element_type_hash,
            )? {
                bail!("array initializer element {idx} type mismatch while lowering");
            }
            operations.extend(value.operations);
            let index_id = ctx.value();
            ctx.push_debug_op(expr_hash, "const_i64", &index_id);
            operations.push(LoweredOp::ConstI64 {
                id: index_id.clone(),
                value: idx.to_string(),
                type_hash: type_hash_for("I64"),
            });
            let element_address = ctx.value();
            ctx.push_debug_op(expr_hash, "addr_of_index", &element_address);
            operations.push(LoweredOp::AddrOfIndex {
                id: element_address.clone(),
                place: LoweredPlace::Index {
                    base: target_address.to_string(),
                    index: index_id,
                    element_type_hash: array_info.element_type_hash.clone(),
                    type_hash: array_info.element_type_hash.clone(),
                },
            });
            operations.push(LoweredOp::Store {
                address: element_address,
                value: value.value,
                type_hash: array_info.element_type_hash.clone(),
            });
        }
        Ok(operations)
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_enum_init_to_address(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        target_type: &str,
        target_address: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<Vec<LoweredOp>> {
        let payload = self.get_payload(expr_hash)?;
        if payload.get("expr_kind").and_then(JsonValue::as_str) != Some("enum_construct") {
            bail!("enum initializer must be enum_construct");
        }
        let declared_type = payload
            .get("enum_type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("enum_construct missing enum_type"))?;
        if !self.type_assignable_in_root(root, declared_type, target_type)? {
            bail!("enum initializer type mismatch while lowering");
        }
        let variant = payload
            .get("variant")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("enum_construct missing variant"))?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("enum_construct missing value"))?;
        let variant_info =
            self.lowered_enum_variant(root, ctx.target_triple(), target_type, variant)?;
        let value = self.lower_expr_as(
            root,
            value_hash,
            &variant_info.type_hash,
            param_types,
            ctx,
            locals,
        )?;
        if !self.type_assignable_in_root(root, &value.type_hash, &variant_info.type_hash)? {
            bail!("enum initializer variant {variant} payload type mismatch while lowering");
        }

        let mut operations = value.operations;
        operations.push(LoweredOp::StoreEnumTag {
            address: target_address.to_string(),
            type_hash: target_type.to_string(),
            variant: variant.to_string(),
            variant_symbol: variant_info.variant_symbol.clone(),
            tag_value: variant_info.tag_value,
        });
        let payload_address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_enum_payload", &payload_address);
        operations.push(LoweredOp::AddrOfEnumPayload {
            id: payload_address.clone(),
            place: LoweredPlace::EnumPayload {
                base: target_address.to_string(),
                variant: variant.to_string(),
                variant_symbol: variant_info.variant_symbol,
                owner_type_hash: target_type.to_string(),
                tag_value: variant_info.tag_value,
                payload_offset_bytes: variant_info.payload_offset_bytes,
                type_hash: variant_info.type_hash.clone(),
            },
        });
        operations.push(LoweredOp::Store {
            address: payload_address,
            value: value.value,
            type_hash: variant_info.type_hash,
        });
        Ok(operations)
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_aggregate_place_init_to_address(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        target_type: &str,
        target_address: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<Vec<LoweredOp>> {
        let source_payload = self.get_payload(expr_hash)?;
        let source_type = expr_type(&source_payload, expr_hash)?;
        if !self.type_assignable_in_root(root, &source_type, target_type)? {
            bail!("aggregate initializer type mismatch while lowering");
        }
        // Enums, fixed arrays, and owned buffers (`vec`/`string`) are whole-slot
        // values with no field-addressable sub-structure, so a `let`-rebind of
        // one initializes the destination by a blind whole-slot copy/move rather
        // than the field-by-field record path below (which would otherwise bail
        // for `vec`/`string`, leaving a program the evaluator accepts but native
        // lowering rejected). Move-only buffers take the `Move` branch and mark
        // the source moved, so the drop scaffold still frees the slot once.
        if self.type_is_enum(root, target_type)?
            || self.type_is_fixed_array(root, target_type)?
            || matches!(
                self.type_spec_in_root(root, target_type)?,
                TypeSpec::Vec { .. } | TypeSpec::String
            )
        {
            if !self.layouts_blind_copy_compatible(
                root,
                ctx.target_triple(),
                &source_type,
                target_type,
            )? {
                bail!("unsupported_aggregate_layout: aggregate initializer layout mismatch");
            }
            let source = self.lower_place(root, expr_hash, param_types, ctx, locals)?;
            let mut operations = source.operations;
            let scaffold_id = ctx.value();
            if self.type_is_move_only(root, ctx.target_triple(), &source_type)? {
                self.mark_moved_source(expr_hash, ctx, locals)?;
                ctx.push_debug_op(expr_hash, "move", &scaffold_id);
                operations.push(LoweredOp::Move {
                    id: scaffold_id.clone(),
                    address: source.address,
                    type_hash: source_type.clone(),
                });
            } else {
                ctx.push_debug_op(expr_hash, "copy", &scaffold_id);
                operations.push(LoweredOp::Copy {
                    id: scaffold_id.clone(),
                    value: source.address,
                    type_hash: source_type.clone(),
                });
            }
            operations.push(LoweredOp::Store {
                address: target_address.to_string(),
                value: scaffold_id,
                type_hash: target_type.to_string(),
            });
            return Ok(operations);
        }
        let source = self.lower_place(root, expr_hash, param_types, ctx, locals)?;
        let mut operations = source.operations;
        let scaffold_id = ctx.value();
        if self.type_is_move_only(root, ctx.target_triple(), &source_type)? {
            self.mark_moved_source(expr_hash, ctx, locals)?;
            ctx.push_debug_op(expr_hash, "move", &scaffold_id);
            operations.push(LoweredOp::Move {
                id: scaffold_id,
                address: source.address.clone(),
                type_hash: source_type.clone(),
            });
        } else {
            ctx.push_debug_op(expr_hash, "copy", &scaffold_id);
            operations.push(LoweredOp::Copy {
                id: scaffold_id,
                value: source.address.clone(),
                type_hash: source_type.clone(),
            });
        }

        for field in self.aggregate_record_fields(root, target_type)? {
            let source_field_info = self.lowered_record_field(
                root,
                ctx.target_triple(),
                &source.type_hash,
                &field.name,
            )?;
            if !self.type_assignable_in_root(
                root,
                &source_field_info.type_hash,
                &field.type_hash,
            )? {
                bail!(
                    "aggregate initializer field {} type mismatch while lowering",
                    field.name
                );
            }

            let source_field_address = ctx.value();
            ctx.push_debug_op(expr_hash, "addr_of_field", &source_field_address);
            operations.push(LoweredOp::AddrOfField {
                id: source_field_address.clone(),
                place: LoweredPlace::Field {
                    base: source.address.clone(),
                    field: field.name.clone(),
                    field_symbol: source_field_info.field_symbol,
                    owner_type_hash: source.type_hash.clone(),
                    offset_bytes: source_field_info.offset_bytes,
                    type_hash: source_field_info.type_hash.clone(),
                },
            });

            let field_value = ctx.value();
            ctx.push_debug_op(expr_hash, "load", &field_value);
            operations.push(LoweredOp::Load {
                id: field_value.clone(),
                address: source_field_address,
                type_hash: source_field_info.type_hash.clone(),
            });

            let target_field_info =
                self.lowered_record_field(root, ctx.target_triple(), target_type, &field.name)?;
            let store_type_hash = if source_field_info.type_hash == target_field_info.type_hash {
                target_field_info.type_hash.clone()
            } else {
                source_field_info.type_hash.clone()
            };
            let target_field_address = ctx.value();
            ctx.push_debug_op(expr_hash, "addr_of_field", &target_field_address);
            operations.push(LoweredOp::AddrOfField {
                id: target_field_address.clone(),
                place: LoweredPlace::Field {
                    base: target_address.to_string(),
                    field: field.name,
                    field_symbol: target_field_info.field_symbol,
                    owner_type_hash: target_type.to_string(),
                    offset_bytes: target_field_info.offset_bytes,
                    type_hash: store_type_hash.clone(),
                },
            });
            operations.push(LoweredOp::Store {
                address: target_field_address,
                value: field_value,
                type_hash: store_type_hash,
            });
        }
        Ok(operations)
    }

    /// Lower a record literal into a fresh stack slot laid out as `slot_type`.
    /// Building the literal directly in the destination type's layout (rather
    /// than its structural, alphabetically-canonicalized type) keeps records
    /// declared in a non-alphabetical field order correct. The safe
    /// `let x: T = { .. }` path already did this; routing every value-flow
    /// boundary through here closes the silent-miscompile holes (record return
    /// values, call arguments, and nested record fields).
    fn lower_record_literal_into_slot(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        slot_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), slot_type)?);
        let slot = ctx.local_slot(slot_type.to_string(), slot_size);
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
        let mut operations = vec![LoweredOp::AddrOfLocal {
            id: address.clone(),
            place: LoweredPlace::Local {
                slot,
                type_hash: slot_type.to_string(),
            },
        }];
        operations.extend(self.lower_record_init_to_address(
            root,
            expr_hash,
            slot_type,
            &address,
            param_types,
            ctx,
            locals,
        )?);
        if self.type_passes_indirect(root, ctx.target_triple(), slot_type)? {
            Ok(LoweredExpr {
                operations,
                value: address,
                type_hash: slot_type.to_string(),
            })
        } else {
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "load", &id);
            operations.push(LoweredOp::Load {
                id: id.clone(),
                address,
                type_hash: slot_type.to_string(),
            });
            Ok(LoweredExpr {
                operations,
                value: id,
                type_hash: slot_type.to_string(),
            })
        }
    }

    fn lower_enum_construct_into_slot(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        slot_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), slot_type)?);
        let slot = ctx.local_slot(slot_type.to_string(), slot_size);
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
        let mut operations = vec![LoweredOp::AddrOfLocal {
            id: address.clone(),
            place: LoweredPlace::Local {
                slot,
                type_hash: slot_type.to_string(),
            },
        }];
        operations.extend(self.lower_enum_init_to_address(
            root,
            expr_hash,
            slot_type,
            &address,
            param_types,
            ctx,
            locals,
        )?);
        if self.type_passes_indirect(root, ctx.target_triple(), slot_type)? {
            Ok(LoweredExpr {
                operations,
                value: address,
                type_hash: slot_type.to_string(),
            })
        } else {
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "load", &id);
            operations.push(LoweredOp::Load {
                id: id.clone(),
                address,
                type_hash: slot_type.to_string(),
            });
            Ok(LoweredExpr {
                operations,
                value: id,
                type_hash: slot_type.to_string(),
            })
        }
    }

    fn lower_array_literal_into_slot(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        slot_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let slot_size =
            stack_slot_size_bytes(self.layout_size_bytes(root, ctx.target_triple(), slot_type)?);
        let slot = ctx.local_slot(slot_type.to_string(), slot_size);
        let address = ctx.value();
        ctx.push_debug_op(expr_hash, "addr_of_local", &address);
        let mut operations = vec![LoweredOp::AddrOfLocal {
            id: address.clone(),
            place: LoweredPlace::Local {
                slot,
                type_hash: slot_type.to_string(),
            },
        }];
        operations.extend(self.lower_array_init_to_address(
            root,
            expr_hash,
            slot_type,
            &address,
            param_types,
            ctx,
            locals,
        )?);
        if self.type_passes_indirect(root, ctx.target_triple(), slot_type)? {
            Ok(LoweredExpr {
                operations,
                value: address,
                type_hash: slot_type.to_string(),
            })
        } else {
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "load", &id);
            operations.push(LoweredOp::Load {
                id: id.clone(),
                address,
                type_hash: slot_type.to_string(),
            });
            Ok(LoweredExpr {
                operations,
                value: id,
                type_hash: slot_type.to_string(),
            })
        }
    }

    /// Lower an early `return <value>` (R7). The operand is lowered to the
    /// function's return type, then — on this early-exit edge — every owned value
    /// still live is dropped (in-scope locals innermost-first, then params), except
    /// the places the operand just consumed (already in `ctx.moved`). This mirrors
    /// exactly what the `let`-scope-exit and function-end param scaffolds drop on
    /// the fall-through path, emitted here instead, so each owned value is dropped
    /// once on the early-exit path (SPEC_V3 §7). The op list ends in `EarlyReturn`,
    /// which makes the enclosing block divergent (`lowered_ops_diverge`); the
    /// returned `value`/`type_hash` serve only as the block's (unread) result.
    fn lower_return(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let value_hash = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("return missing value"))?;
        let return_type = ctx.return_type.clone();
        let target_triple = ctx.target_triple().to_string();
        // Lower the operand to the function's return type. A mismatch here is the
        // authoritative operand-type gate for `return` (the type-checker does not
        // thread the return type into operand positions).
        let value =
            self.lower_expr_as(root, value_hash, &return_type, param_types, ctx, locals)?;
        if !self.type_assignable_in_root(root, &value.type_hash, &return_type)? {
            bail!(
                "return value type {} does not match function return type {}",
                value.type_hash,
                return_type
            );
        }
        let mut operations = value.operations;
        // Drop the owned values live at the exit: in-scope locals innermost-first,
        // then params, each respecting the places the operand consumed.
        for binding in locals.iter().rev() {
            if self.type_requires_drop_scaffold(root, &target_triple, &binding.type_hash)? {
                let moved = ctx.moved.clone();
                let place = MovedPlace::whole(RootSlot::Local(binding.slot));
                self.emit_residual_drops(
                    root,
                    &place,
                    &binding.type_hash,
                    &binding.type_hash,
                    &moved,
                    value_hash,
                    ctx,
                    &mut operations,
                )?;
            }
        }
        operations.extend(self.lower_param_drop_scaffolds(
            root,
            &target_triple,
            param_types,
            value_hash,
            ctx,
        )?);
        operations.push(LoweredOp::EarlyReturn {
            value: value.value.clone(),
            type_hash: return_type.clone(),
        });
        Ok(LoweredExpr {
            operations,
            value: value.value,
            type_hash: return_type,
        })
    }

    #[allow(clippy::too_many_arguments)]
    /// Lower an `if`, producing a value of `result_type`. Like `lower_case`,
    /// `result_type` is the expression's own type on the ordinary path, but
    /// `lower_expr_as` may pass an enclosing expected type so literal branches
    /// build directly in the destination layout.
    fn lower_if(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        result_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
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
        let cond = self.lower_expr(root, cond_hash, param_types, ctx, locals)?;
        if cond.type_hash != type_hash_for("Bool") {
            bail!("if condition must lower to bool");
        }
        // `if` branches are alternative paths, so a value moved in one branch is
        // live (and must still be dropped) on the other. Conditional drop glue
        // (SPEC_V3 §7): track moves per branch, then normalize both branches to
        // the union of their outer moves by emitting compensating drops in the
        // branch that left a unioned place live. Every path then exits the `if`
        // with the same move set, so the static scaffold drops each owned value
        // exactly once — no runtime drop flags. Branch-local temporaries (slots
        // minted at/above `locals_boundary`) drop within their own branch.
        let locals_boundary = ctx.next_local;
        let moved_before = ctx.moved.clone();
        let mut then_expr =
            self.lower_expr_as(root, then_hash, result_type, param_types, ctx, locals)?;
        let then_moved = outer_branch_moves(&ctx.moved, &moved_before, locals_boundary);
        ctx.moved = moved_before.clone();
        let mut else_expr =
            self.lower_expr_as(root, else_hash, result_type, param_types, ctx, locals)?;
        let else_moved = outer_branch_moves(&ctx.moved, &moved_before, locals_boundary);
        // Early exit (R7): a branch that always `return`s exits through its own
        // `EarlyReturn` (which already dropped every value live at that point) and
        // never reaches the merge. So a divergent branch (a) yields the return
        // type, not `result_type`, so it is exempt from the branch-type check; and
        // (b) contributes no moves to the continuation and receives no compensating
        // drops — only the non-divergent branch(es) merge. With one branch
        // divergent the post-`if` state is the other branch's; with both, the code
        // after the `if` is unreachable.
        let then_div = lowered_ops_diverge(&then_expr.operations);
        let else_div = lowered_ops_diverge(&else_expr.operations);
        if !then_div && then_expr.type_hash != result_type {
            bail!("if branch type mismatch while lowering");
        }
        if !else_div && else_expr.type_hash != result_type {
            bail!("if branch type mismatch while lowering");
        }
        ctx.moved = moved_before.clone();
        match (then_div, else_div) {
            (false, false) => {
                let union =
                    normalize_moved_set(then_moved.union(&else_moved).cloned().collect());
                self.emit_merge_compensation(
                    root,
                    &union,
                    &then_moved,
                    param_types,
                    then_hash,
                    ctx,
                    &mut then_expr.operations,
                )?;
                self.emit_merge_compensation(
                    root,
                    &union,
                    &else_moved,
                    param_types,
                    else_hash,
                    ctx,
                    &mut else_expr.operations,
                )?;
                for place in union {
                    ctx.mark_moved_place(place);
                }
            }
            (true, false) => {
                for place in else_moved {
                    ctx.mark_moved_place(place);
                }
            }
            (false, true) => {
                for place in then_moved {
                    ctx.mark_moved_place(place);
                }
            }
            (true, true) => {}
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "if", &id);
        let mut operations = cond.operations;
        operations.push(LoweredOp::If {
            id: id.clone(),
            cond: cond.value,
            then_block: LoweredBlock {
                operations: then_expr.operations,
                result: then_expr.value,
            },
            else_block: LoweredBlock {
                operations: else_expr.operations,
                result: else_expr.value,
            },
            type_hash: result_type.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: result_type.to_string(),
        })
    }

    /// Lower a `case`, producing a value of `result_type`. `result_type` is the
    /// case's own (typed-DAG) type on the ordinary path, but `lower_expr_as` may
    /// pass an enclosing expected type so record/enum/array-literal arms build
    /// directly in the destination layout (the field-order-safe path).
    fn lower_case(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        result_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let scrutinee_hash = payload
            .get("expr")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("case missing expr"))?;
        let scrutinee = self.lower_expr(root, scrutinee_hash, param_types, ctx, locals)?;
        let arms = payload
            .get("arms")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("case missing arms"))?;
        if arms.is_empty() {
            bail!("case lowering requires at least one arm");
        }
        // Scalar literal `case` (R14): desugar to an `if`/`eq` chain reusing the
        // existing backend (no new code generation). Conditional drop glue
        // (SPEC_V3 §7) handles any owned values consumed in the arm bodies.
        if scrutinee.type_hash == type_hash_for("I64") {
            return self.lower_scalar_i64_case(
                root,
                scrutinee,
                arms,
                result_type,
                expr_hash,
                param_types,
                ctx,
                locals,
            );
        }
        if scrutinee.type_hash == type_hash_for("Bool") {
            return self.lower_scalar_bool_case(
                root,
                scrutinee,
                arms,
                result_type,
                expr_hash,
                param_types,
                ctx,
                locals,
            );
        }
        // Enum `case` (R14): collect the non-default arms (each a pattern node
        // carrying its body) and the optional default body, then dispatch —
        // recursively for nested destructuring patterns.
        let mut node_arms: Vec<(&JsonValue, &str)> = Vec::new();
        let mut fallback: Option<&str> = None;
        for arm in arms {
            if arm.get("default").and_then(JsonValue::as_bool) == Some(true) {
                fallback = Some(
                    arm.get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?,
                );
                continue;
            }
            let body = arm
                .get("body")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("case arm missing body"))?;
            node_arms.push((arm, body));
        }
        // A move-only payload can be moved out of a case arm only from a consumed
        // (param/local) scrutinee; nested levels inherit this (an inner payload lives
        // inside the consumed outer enum).
        let scrutinee_consumed = matches!(
            self.get_payload(scrutinee_hash)?
                .get("expr_kind")
                .and_then(JsonValue::as_str),
            Some("param_ref" | "local_ref")
        );
        let mut operations = scrutinee.operations;
        let dispatch = self.lower_enum_dispatch(
            root,
            expr_hash,
            &scrutinee.value,
            &scrutinee.type_hash,
            scrutinee_consumed,
            &node_arms,
            fallback,
            result_type,
            param_types,
            ctx,
            locals,
        )?;
        operations.extend(dispatch.operations);
        Ok(LoweredExpr {
            operations,
            value: dispatch.value,
            type_hash: result_type.to_string(),
        })
    }

    /// Lower an enum dispatch (R14): match `scrutinee_value` (a *pointer* to an enum
    /// of `scrutinee_type`) against `node_arms` — each a `(pattern node, body)` pair
    /// whose node is `{"variant", "binding_name"?, "payload_pattern"?}` — falling
    /// back to `fallback` (the case `_`/default body, which binds nothing) for any
    /// uncovered variant. `scrutinee_consumed` is true when the scrutinee is a
    /// consumed place, so a move-only payload has a single owner to move from.
    ///
    /// For a nested-destructuring group the payload's *address* becomes the inner
    /// scrutinee and this recurses; the inner enum shell lives inside the consumed
    /// outer enum (never separately dropped), so each level is an ordinary tag switch
    /// and the per-level binding + residual/merge drop glue keeps every owned value
    /// dropped exactly once (SPEC_V3 §7).
    #[allow(clippy::too_many_arguments)]
    fn lower_enum_dispatch(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        scrutinee_value: &str,
        scrutinee_type: &str,
        scrutinee_consumed: bool,
        node_arms: &[(&JsonValue, &str)],
        fallback: Option<&str>,
        result_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let TypeSpec::Enum(variants) = self.type_spec_in_root(root, scrutinee_type)? else {
            bail!("case lowering requires enum or scalar scrutinee");
        };
        // Group arms by variant in first-appearance order (a nested variant may carry
        // several arms, dispatched on its payload), then append the default-filled
        // missing variants in declaration order — preserving the previous arm order.
        let mut groups: Vec<(String, Vec<(&JsonValue, &str)>)> = Vec::new();
        for &(node, body) in node_arms {
            let variant = node
                .get("variant")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("case arm missing variant"))?
                .to_string();
            if let Some(group) = groups.iter_mut().find(|(name, _)| *name == variant) {
                group.1.push((node, body));
            } else {
                groups.push((variant, vec![(node, body)]));
            }
        }
        let covered: BTreeSet<String> = groups.iter().map(|(name, _)| name.clone()).collect();
        for variant in &variants {
            if !covered.contains(&variant.name) {
                groups.push((variant.name.clone(), Vec::new()));
            }
        }

        let locals_boundary = ctx.next_local;
        let moved_before = ctx.moved.clone();
        let mut arm_move_sets: Vec<BTreeSet<MovedPlace>> = Vec::with_capacity(groups.len());
        let mut lowered_arms = Vec::with_capacity(groups.len());
        for (variant, group) in groups {
            ctx.moved = moved_before.clone();
            let variant_info =
                self.lowered_enum_variant(root, ctx.target_triple(), scrutinee_type, &variant)?;
            let mut arm_operations = Vec::new();
            // A nested-destructuring group: every arm carries a `payload_pattern`
            // (well-formedness, enforced in type-check, makes a group homogeneous).
            let nested = group
                .first()
                .is_some_and(|(node, _)| node.get("payload_pattern").is_some());
            let result = if nested {
                // Bind the payload's address and recurse on it.
                let payload_address = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_enum_payload", &payload_address);
                arm_operations.push(LoweredOp::AddrOfEnumPayload {
                    id: payload_address.clone(),
                    place: LoweredPlace::EnumPayload {
                        base: scrutinee_value.to_string(),
                        variant: variant.clone(),
                        variant_symbol: variant_info.variant_symbol.clone(),
                        owner_type_hash: scrutinee_type.to_string(),
                        tag_value: variant_info.tag_value,
                        payload_offset_bytes: variant_info.payload_offset_bytes,
                        type_hash: variant_info.type_hash.clone(),
                    },
                });
                let inner_arms: Vec<(&JsonValue, &str)> = group
                    .iter()
                    .map(|(node, body)| {
                        Ok((
                            node.get("payload_pattern")
                                .ok_or_else(|| anyhow!("nested case arm missing payload_pattern"))?,
                            *body,
                        ))
                    })
                    .collect::<Result<_>>()?;
                let inner = self.lower_enum_dispatch(
                    root,
                    expr_hash,
                    &payload_address,
                    &variant_info.type_hash,
                    scrutinee_consumed,
                    &inner_arms,
                    fallback,
                    result_type,
                    param_types,
                    ctx,
                    locals,
                )?;
                arm_operations.extend(inner.operations);
                inner.value
            } else if let Some(binding) = group
                .first()
                .and_then(|(node, _)| node.get("binding_name").and_then(JsonValue::as_str))
            {
                let _ = binding;
                let body_hash = group[0].1;
                let binding_is_move_only =
                    self.type_is_move_only(root, ctx.target_triple(), &variant_info.type_hash)?;
                // Moving a move-only payload out of a case arm is sound only when the
                // scrutinee is itself consumed, so the payload's single owner becomes
                // the binding and the scrutinee is never dropped (no double free). The
                // payload then transfers to the binding by a shallow read of the
                // (abandoned) enum storage: a `box<T>` payload by loading its pointer,
                // an inline move-only aggregate by `Load`-aliasing the payload pointer
                // and `Store`-memcpying it into the binding slot (a byte move — inner
                // owned pointers transfer; the consumed scrutinee is never dropped, so
                // each resource is freed once, SPEC_V3 §7). A non-place (temporary)
                // scrutinee stays fail-closed: there is no single consumed owner.
                if binding_is_move_only && !scrutinee_consumed {
                    bail!(
                        "unsupported_move: moving a move-only enum payload out of a case arm requires a consumed (param/local) scrutinee; moving out of a temporary enum is not yet supported (SPEC_V3 §7)"
                    );
                }
                let slot_size = stack_slot_size_bytes(self.layout_size_bytes(
                    root,
                    ctx.target_triple(),
                    &variant_info.type_hash,
                )?);
                let slot = ctx.local_slot(variant_info.type_hash.clone(), slot_size);
                let payload_address = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_enum_payload", &payload_address);
                arm_operations.push(LoweredOp::AddrOfEnumPayload {
                    id: payload_address.clone(),
                    place: LoweredPlace::EnumPayload {
                        base: scrutinee_value.to_string(),
                        variant: variant.clone(),
                        variant_symbol: variant_info.variant_symbol.clone(),
                        owner_type_hash: scrutinee_type.to_string(),
                        tag_value: variant_info.tag_value,
                        payload_offset_bytes: variant_info.payload_offset_bytes,
                        type_hash: variant_info.type_hash.clone(),
                    },
                });
                let payload_value = if binding_is_move_only {
                    let pv = ctx.value();
                    ctx.push_debug_op(expr_hash, "load", &pv);
                    arm_operations.push(LoweredOp::Load {
                        id: pv.clone(),
                        address: payload_address.clone(),
                        type_hash: variant_info.type_hash.clone(),
                    });
                    pv
                } else {
                    self.lower_copy_value_from_address(
                        root,
                        expr_hash,
                        &variant_info.type_hash,
                        &payload_address,
                        ctx,
                        &mut arm_operations,
                    )?
                };
                let local_address = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_local", &local_address);
                arm_operations.push(LoweredOp::AddrOfLocal {
                    id: local_address.clone(),
                    place: LoweredPlace::Local {
                        slot,
                        type_hash: variant_info.type_hash.clone(),
                    },
                });
                arm_operations.push(LoweredOp::Store {
                    address: local_address,
                    value: payload_value,
                    type_hash: variant_info.type_hash.clone(),
                });
                locals.push(LocalLoweredBinding {
                    slot,
                    type_hash: variant_info.type_hash.clone(),
                });
                let body =
                    self.lower_expr_as(root, body_hash, result_type, param_types, ctx, locals);
                locals.pop();
                let body = body?;
                if body.type_hash != result_type {
                    bail!("case arm {variant} result type mismatch while lowering");
                }
                arm_operations.extend(body.operations);
                // Drop the binding at arm-scope exit if the body did not consume it
                // (mirrors `let`-binding drop placement, SPEC_V3 §7).
                if self.type_requires_drop_scaffold(
                    root,
                    ctx.target_triple(),
                    &variant_info.type_hash,
                )? {
                    let moved = ctx.moved.clone();
                    let place = MovedPlace::whole(RootSlot::Local(slot));
                    self.emit_residual_drops(
                        root,
                        &place,
                        &variant_info.type_hash,
                        &variant_info.type_hash,
                        &moved,
                        body_hash,
                        ctx,
                        &mut arm_operations,
                    )?;
                }
                body.value
            } else {
                // No-binding arm: either a simple `_`-ignored / unit-payload arm, or a
                // default-filled missing variant (empty group → the `fallback` body).
                // The body binds nothing; a drop-requiring payload is freed here (the
                // matched variant's payload would otherwise leak, since a move-only
                // scrutinee is consumed — the sole free, exactly once, SPEC_V3 §7).
                let body_hash = match group.first() {
                    Some((_, body)) => *body,
                    None => fallback.ok_or_else(|| {
                        anyhow!("non-exhaustive case: variant {variant} has no arm and no default")
                    })?,
                };
                let body =
                    self.lower_expr_as(root, body_hash, result_type, param_types, ctx, locals)?;
                if body.type_hash != result_type {
                    bail!("case arm {variant} result type mismatch while lowering");
                }
                arm_operations.extend(body.operations);
                if self.type_requires_drop_scaffold(
                    root,
                    ctx.target_triple(),
                    &variant_info.type_hash,
                )? {
                    let payload_address = ctx.value();
                    ctx.push_debug_op(expr_hash, "addr_of_enum_payload", &payload_address);
                    arm_operations.push(LoweredOp::AddrOfEnumPayload {
                        id: payload_address.clone(),
                        place: LoweredPlace::EnumPayload {
                            base: scrutinee_value.to_string(),
                            variant: variant.clone(),
                            variant_symbol: variant_info.variant_symbol.clone(),
                            owner_type_hash: scrutinee_type.to_string(),
                            tag_value: variant_info.tag_value,
                            payload_offset_bytes: variant_info.payload_offset_bytes,
                            type_hash: variant_info.type_hash.clone(),
                        },
                    });
                    arm_operations.push(LoweredOp::Drop {
                        address: payload_address,
                        type_hash: variant_info.type_hash.clone(),
                    });
                }
                body.value
            };

            lowered_arms.push(LoweredCaseArm {
                variant,
                variant_symbol: variant_info.variant_symbol,
                tag_value: variant_info.tag_value,
                payload_type_hash: variant_info.type_hash,
                payload_offset_bytes: variant_info.payload_offset_bytes,
                block: LoweredBlock {
                    operations: arm_operations,
                    result,
                },
            });

            let arm_moves = outer_branch_moves(&ctx.moved, &moved_before, locals_boundary);
            arm_move_sets.push(arm_moves);
        }
        // Conditional drop glue (SPEC_V3 §7): normalize every arm to the union of
        // their outer moves by emitting compensating drops in arms that left a unioned
        // place live, so the static scaffold drops each owned value exactly once across
        // all arms — no runtime drop flags. Early exit (R7): a divergent arm exits
        // via its own `EarlyReturn` (already dropping every value live there) and
        // never reaches the merge, so it is excluded from the union and skips
        // compensation — only non-divergent arms merge.
        let arm_diverges: Vec<bool> = lowered_arms
            .iter()
            .map(|arm| lowered_block_diverges(&arm.block))
            .collect();
        let mut union = BTreeSet::new();
        for (idx, arm_moves) in arm_move_sets.iter().enumerate() {
            if arm_diverges[idx] {
                continue;
            }
            for place in arm_moves {
                union.insert(place.clone());
            }
        }
        let union = normalize_moved_set(union);
        for (idx, (arm, arm_moves)) in
            lowered_arms.iter_mut().zip(arm_move_sets.iter()).enumerate()
        {
            if arm_diverges[idx] {
                continue;
            }
            self.emit_merge_compensation(
                root,
                &union,
                arm_moves,
                param_types,
                expr_hash,
                ctx,
                &mut arm.block.operations,
            )?;
        }
        ctx.moved = moved_before;
        for place in union {
            ctx.mark_moved_place(place);
        }

        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "case", &id);
        Ok(LoweredExpr {
            operations: vec![LoweredOp::Case {
                id: id.clone(),
                scrutinee: scrutinee_value.to_string(),
                enum_type_hash: scrutinee_type.to_string(),
                arms: lowered_arms,
                type_hash: result_type.to_string(),
            }],
            value: id,
            type_hash: result_type.to_string(),
        })
    }

    /// Lower a scalar `i64` literal `case` (R14) by desugaring to an `if`/`eq_i64`
    /// chain — reusing the existing backend with no new code generation.
    #[allow(clippy::too_many_arguments)]
    fn lower_scalar_i64_case(
        &self,
        root: &ProgramRootPayload,
        scrutinee: LoweredExpr,
        arms: &[JsonValue],
        result_type: &str,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        // Each conditional arm carries its pattern, an optional guard (R14) hash, and
        // its body; the single UNGUARDED wildcard becomes the chain's terminal `else`.
        let mut chain_arms: Vec<(ScalarArmPattern, Option<String>, String)> = Vec::new();
        let mut default_body: Option<String> = None;
        for arm in arms {
            let body = arm
                .get("body")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("case arm missing body"))?
                .to_string();
            let guard = arm
                .get("guard")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            if arm.get("default").and_then(JsonValue::as_bool) == Some(true) {
                match guard {
                    // A guarded wildcard is a conditional arm (its only test is the
                    // guard); the unguarded wildcard is the terminal default.
                    Some(_) => chain_arms.push((ScalarArmPattern::Wildcard, guard, body)),
                    None => default_body = Some(body),
                }
            } else if let Some(value) = arm.get("literal_i64").and_then(JsonValue::as_str) {
                chain_arms.push((ScalarArmPattern::Literal(value.to_string()), guard, body));
            } else if let Some(lo) = arm.get("range_lo").and_then(JsonValue::as_str) {
                let hi = arm
                    .get("range_hi")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("range case arm missing range_hi"))?;
                let inclusive = arm
                    .get("range_inclusive")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                chain_arms.push((
                    ScalarArmPattern::Range {
                        lo: lo.to_string(),
                        hi: hi.to_string(),
                        inclusive,
                    },
                    guard,
                    body,
                ));
            } else {
                bail!("scalar i64 case arm must be an integer literal, range, or `_`");
            }
        }
        let default_body = default_body
            .ok_or_else(|| anyhow!("scalar i64 case lowering requires a `_` wildcard arm"))?;
        let mut operations = scrutinee.operations;
        let moved_before = ctx.moved.clone();
        let chain = self.lower_scalar_i64_chain(
            root,
            &scrutinee.value,
            &chain_arms,
            &default_body,
            result_type,
            expr_hash,
            param_types,
            ctx,
            locals,
            &moved_before,
            0,
        )?;
        operations.extend(chain.operations);
        Ok(LoweredExpr {
            operations,
            value: chain.value,
            type_hash: result_type.to_string(),
        })
    }

    /// Build the `if`/`eq` chain for [`lower_scalar_i64_case`], one binary `if` per
    /// conditional arm (literal / range / guarded wildcard) with the unguarded
    /// wildcard body as the final `else`. Conditional drop glue (SPEC_V3 §7) is
    /// applied per `if` level exactly as in `lower_if`.
    #[allow(clippy::too_many_arguments)]
    fn lower_scalar_i64_chain(
        &self,
        root: &ProgramRootPayload,
        scrutinee_value: &str,
        chain_arms: &[(ScalarArmPattern, Option<String>, String)],
        default_body: &str,
        result_type: &str,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
        moved_before: &BTreeSet<MovedPlace>,
        index: usize,
    ) -> Result<LoweredExpr> {
        let Some((pattern, guard, body_hash)) = chain_arms.get(index) else {
            // Final `else`: the unguarded wildcard arm.
            ctx.moved = moved_before.clone();
            let body = self.lower_expr_as(root, default_body, result_type, param_types, ctx, locals)?;
            if body.type_hash != result_type {
                bail!("scalar case arm result type mismatch while lowering");
            }
            return Ok(body);
        };
        // pattern_test = the arm's match test against the scrutinee (`None` for a
        // wildcard, which always matches). A literal compares with `==`; a range
        // desugars to `scrutinee >= lo && scrutinee {<,<=} hi`.
        let mut operations = Vec::new();
        let i64_type = type_hash_for("I64");
        let bool_type = type_hash_for("Bool");
        let const_i64 = |ctx: &mut LowerCtx, value: &str, ops: &mut Vec<LoweredOp>| {
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "const_i64", &id);
            ops.push(LoweredOp::ConstI64 {
                id: id.clone(),
                value: value.to_string(),
                type_hash: i64_type.clone(),
            });
            id
        };
        let pattern_test: Option<String> = match pattern {
            ScalarArmPattern::Literal(value) => {
                let const_id = const_i64(ctx, value, &mut operations);
                let cond_id = ctx.value();
                ctx.push_debug_op(expr_hash, "binary", &cond_id);
                operations.push(LoweredOp::Binary {
                    id: cond_id.clone(),
                    kind: lower_binary_kind("==", &i64_type, &i64_type, &bool_type)?,
                    left: scrutinee_value.to_string(),
                    right: const_id,
                    type_hash: bool_type.clone(),
                    trap: None,
                });
                Some(cond_id)
            }
            ScalarArmPattern::Range { lo, hi, inclusive } => {
                let lo_id = const_i64(ctx, lo, &mut operations);
                let ge_id = ctx.value();
                ctx.push_debug_op(expr_hash, "binary", &ge_id);
                operations.push(LoweredOp::Binary {
                    id: ge_id.clone(),
                    kind: lower_binary_kind(">=", &i64_type, &i64_type, &bool_type)?,
                    left: scrutinee_value.to_string(),
                    right: lo_id,
                    type_hash: bool_type.clone(),
                    trap: None,
                });
                let hi_id = const_i64(ctx, hi, &mut operations);
                let upper = if *inclusive { "<=" } else { "<" };
                let le_id = ctx.value();
                ctx.push_debug_op(expr_hash, "binary", &le_id);
                operations.push(LoweredOp::Binary {
                    id: le_id.clone(),
                    kind: lower_binary_kind(upper, &i64_type, &i64_type, &bool_type)?,
                    left: scrutinee_value.to_string(),
                    right: hi_id,
                    type_hash: bool_type.clone(),
                    trap: None,
                });
                let cond_id = ctx.value();
                ctx.push_debug_op(expr_hash, "binary", &cond_id);
                operations.push(LoweredOp::Binary {
                    id: cond_id.clone(),
                    kind: lower_binary_kind("&&", &bool_type, &bool_type, &bool_type)?,
                    left: ge_id,
                    right: le_id,
                    type_hash: bool_type.clone(),
                    trap: None,
                });
                Some(cond_id)
            }
            ScalarArmPattern::Wildcard => None,
        };
        // Fold the guard (R14) into the arm condition with SHORT-CIRCUIT semantics:
        // the guard must run only when the pattern matched. A pure guard can still
        // TRAP (e.g. `100/y`), and the evaluator skips an unmatched arm's guard, so an
        // eager strict-`&&` (`and_bool` pre-computes both operands) would diverge.
        // Express the short-circuit as a bool-valued `if`:
        //   guarded literal/range   -> if pattern_test { guard } else { false }
        //   guarded wildcard        -> guard          (the arm is always reached)
        //   unguarded literal/range -> pattern_test
        let cond_id = match guard {
            Some(guard_hash) => {
                let moved_snapshot = ctx.moved.clone();
                let guard_lowered =
                    self.lower_expr_as(root, guard_hash, &bool_type, param_types, ctx, locals)?;
                if guard_lowered.type_hash != bool_type {
                    bail!("case guard must lower to bool");
                }
                // The guard must not consume owned values: its conditional,
                // short-circuit evaluation would otherwise make drop placement
                // path-dependent. Fail closed (move in the arm body instead).
                if ctx.moved != moved_snapshot {
                    bail!("case guard may not move owned values");
                }
                match pattern_test {
                    Some(pattern_id) => {
                        let false_id = ctx.value();
                        ctx.push_debug_op(expr_hash, "const_bool", &false_id);
                        let if_id = ctx.value();
                        ctx.push_debug_op(expr_hash, "if", &if_id);
                        operations.push(LoweredOp::If {
                            id: if_id.clone(),
                            cond: pattern_id,
                            then_block: LoweredBlock {
                                operations: guard_lowered.operations,
                                result: guard_lowered.value,
                            },
                            else_block: LoweredBlock {
                                operations: vec![LoweredOp::ConstBool {
                                    id: false_id.clone(),
                                    value: false,
                                    type_hash: bool_type.clone(),
                                }],
                                result: false_id,
                            },
                            type_hash: bool_type.clone(),
                        });
                        if_id
                    }
                    None => {
                        // Guarded wildcard: always reached, so evaluate the guard
                        // directly (mirrors the evaluator evaluating a reached
                        // wildcard's guard).
                        operations.extend(guard_lowered.operations);
                        guard_lowered.value
                    }
                }
            }
            None => match pattern_test {
                Some(pattern_id) => pattern_id,
                None => bail!("scalar i64 case wildcard chain arm requires a guard"),
            },
        };
        let locals_boundary = ctx.next_local;
        // then = this arm's body
        ctx.moved = moved_before.clone();
        let mut then_expr = self.lower_expr_as(root, body_hash, result_type, param_types, ctx, locals)?;
        let then_div = lowered_ops_diverge(&then_expr.operations);
        if !then_div && then_expr.type_hash != result_type {
            bail!("scalar case arm result type mismatch while lowering");
        }
        let then_moved = outer_branch_moves(&ctx.moved, moved_before, locals_boundary);
        // else = the rest of the chain
        ctx.moved = moved_before.clone();
        let mut else_expr = self.lower_scalar_i64_chain(
            root,
            scrutinee_value,
            chain_arms,
            default_body,
            result_type,
            expr_hash,
            param_types,
            ctx,
            locals,
            moved_before,
            index + 1,
        )?;
        let else_moved = outer_branch_moves(&ctx.moved, moved_before, locals_boundary);
        // Early exit (R7): merge only non-divergent branches (see `lower_if`).
        self.merge_two_branch_moves(
            root,
            &mut then_expr,
            &mut else_expr,
            then_moved,
            else_moved,
            moved_before,
            param_types,
            expr_hash,
            ctx,
        )?;
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "if", &id);
        operations.push(LoweredOp::If {
            id: id.clone(),
            cond: cond_id,
            then_block: LoweredBlock {
                operations: then_expr.operations,
                result: then_expr.value,
            },
            else_block: LoweredBlock {
                operations: else_expr.operations,
                result: else_expr.value,
            },
            type_hash: result_type.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: result_type.to_string(),
        })
    }

    /// Lower a scalar `bool` literal `case` (R14) to a single `if` (no `eq` —
    /// bool has no equality operator; the scrutinee IS the condition).
    #[allow(clippy::too_many_arguments)]
    fn lower_scalar_bool_case(
        &self,
        root: &ProgramRootPayload,
        scrutinee: LoweredExpr,
        arms: &[JsonValue],
        result_type: &str,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let mut true_body: Option<String> = None;
        let mut false_body: Option<String> = None;
        let mut default_body: Option<String> = None;
        for arm in arms {
            let body = arm
                .get("body")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("case arm missing body"))?
                .to_string();
            if arm.get("default").and_then(JsonValue::as_bool) == Some(true) {
                default_body = Some(body);
            } else if let Some(value) = arm.get("literal_bool").and_then(JsonValue::as_bool) {
                if value {
                    true_body = Some(body);
                } else {
                    false_body = Some(body);
                }
            } else {
                bail!("scalar bool case arm must be a bool literal or `_`");
            }
        }
        let then_body = true_body
            .or_else(|| default_body.clone())
            .ok_or_else(|| anyhow!("scalar bool case is missing a true/wildcard arm"))?;
        let else_body = false_body
            .or(default_body)
            .ok_or_else(|| anyhow!("scalar bool case is missing a false/wildcard arm"))?;

        let mut operations = scrutinee.operations;
        let moved_before = ctx.moved.clone();
        let locals_boundary = ctx.next_local;
        ctx.moved = moved_before.clone();
        let mut then_expr = self.lower_expr_as(root, &then_body, result_type, param_types, ctx, locals)?;
        let then_div = lowered_ops_diverge(&then_expr.operations);
        if !then_div && then_expr.type_hash != result_type {
            bail!("scalar case arm result type mismatch while lowering");
        }
        let then_moved = outer_branch_moves(&ctx.moved, &moved_before, locals_boundary);
        ctx.moved = moved_before.clone();
        let mut else_expr = self.lower_expr_as(root, &else_body, result_type, param_types, ctx, locals)?;
        let else_div = lowered_ops_diverge(&else_expr.operations);
        if !else_div && else_expr.type_hash != result_type {
            bail!("scalar case arm result type mismatch while lowering");
        }
        let else_moved = outer_branch_moves(&ctx.moved, &moved_before, locals_boundary);
        // Early exit (R7): merge only non-divergent branches (see `lower_if`).
        self.merge_two_branch_moves(
            root,
            &mut then_expr,
            &mut else_expr,
            then_moved,
            else_moved,
            &moved_before,
            param_types,
            expr_hash,
            ctx,
        )?;
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "if", &id);
        operations.push(LoweredOp::If {
            id: id.clone(),
            cond: scrutinee.value,
            then_block: LoweredBlock {
                operations: then_expr.operations,
                result: then_expr.value,
            },
            else_block: LoweredBlock {
                operations: else_expr.operations,
                result: else_expr.value,
            },
            type_hash: result_type.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: result_type.to_string(),
        })
    }

    fn lower_copy_value_from_address(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        address: &str,
        ctx: &mut LowerCtx,
        operations: &mut Vec<LoweredOp>,
    ) -> Result<String> {
        if self.type_passes_indirect(root, ctx.target_triple(), type_hash)? {
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "copy", &id);
            operations.push(LoweredOp::Copy {
                id: id.clone(),
                value: address.to_string(),
                type_hash: type_hash.to_string(),
            });
            return Ok(id);
        }
        let loaded_id = ctx.value();
        ctx.push_debug_op(expr_hash, "load", &loaded_id);
        operations.push(LoweredOp::Load {
            id: loaded_id.clone(),
            address: address.to_string(),
            type_hash: type_hash.to_string(),
        });
        if self.type_requires_copy_scaffold(root, ctx.target_triple(), type_hash)? {
            let copy_id = ctx.value();
            ctx.push_debug_op(expr_hash, "copy", &copy_id);
            operations.push(LoweredOp::Copy {
                id: copy_id.clone(),
                value: loaded_id,
                type_hash: type_hash.to_string(),
            });
            Ok(copy_id)
        } else {
            Ok(loaded_id)
        }
    }

    /// Lower `expr_hash` so its result is usable where `expected_type` is
    /// required. A record literal is built directly in `expected_type`'s layout.
    /// Any other value whose static type differs from `expected_type` is allowed
    /// only when a verbatim byte copy reinterpreted under the destination layout
    /// is provably sound; otherwise lowering fails closed instead of silently
    /// reordering fields. This closes the structural-vs-nominal record layout
    /// hole at every value-flow boundary.
    fn lower_expr_as(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        expected_type: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let is_record_literal = self
            .get_payload(expr_hash)?
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            == Some("record_literal");
        if is_record_literal && self.type_is_record(root, expected_type)? {
            return self.lower_record_literal_into_slot(
                root,
                expr_hash,
                expected_type,
                param_types,
                ctx,
                locals,
            );
        }
        let is_enum_construct = self
            .get_payload(expr_hash)?
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            == Some("enum_construct");
        if is_enum_construct && self.type_is_enum(root, expected_type)? {
            return self.lower_enum_construct_into_slot(
                root,
                expr_hash,
                expected_type,
                param_types,
                ctx,
                locals,
            );
        }
        let is_array_literal = matches!(
            self.get_payload(expr_hash)?
                .get("expr_kind")
                .and_then(JsonValue::as_str),
            Some("array_literal") | Some("array_fill") | Some("array_set")
        );
        if is_array_literal && self.type_is_fixed_array(root, expected_type)? {
            return self.lower_array_literal_into_slot(
                root,
                expr_hash,
                expected_type,
                param_types,
                ctx,
                locals,
            );
        }
        if self
            .get_payload(expr_hash)?
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            == Some("box_new")
            && self.type_is_box(root, expected_type)?
        {
            return self.lower_box_new_as(root, expr_hash, expected_type, param_types, ctx, locals);
        }
        // `case`/`if` produce their result from per-arm/branch values. Propagate
        // the expected type into those arms so a record/enum/array-literal arm
        // builds directly in the destination layout, instead of being assembled
        // in its structural (alphabetical) type and then blind-copied. Only
        // retarget when the construct's own type is assignable to `expected_type`
        // but NOT byte-compatible with it — i.e. exactly the differing-field-order
        // case that fails closed today. When the layouts are blind-copy compatible
        // the existing copy already works, so keep the own type and do not change
        // (or risk regressing) that path.
        let kind = self
            .get_payload(expr_hash)?
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        if matches!(kind.as_deref(), Some("case") | Some("if")) {
            let own_type = expr_type(&self.get_payload(expr_hash)?, expr_hash)?;
            let build_as = if own_type != expected_type
                && self.type_assignable_in_root(root, &own_type, expected_type)?
                && !self.layouts_blind_copy_compatible(
                    root,
                    ctx.target_triple(),
                    &own_type,
                    expected_type,
                )? {
                expected_type
            } else {
                own_type.as_str()
            };
            let lowered = if kind.as_deref() == Some("case") {
                self.lower_case(root, expr_hash, build_as, param_types, ctx, locals)?
            } else {
                self.lower_if(root, expr_hash, build_as, param_types, ctx, locals)?
            };
            // Safety net identical to the generic path below: a result we did not
            // build into the destination must still be a sound verbatim byte copy.
            if lowered.type_hash != expected_type
                && (self.type_is_record(root, &lowered.type_hash)?
                    || self.type_is_record(root, expected_type)?
                    || self.type_is_enum(root, &lowered.type_hash)?
                    || self.type_is_enum(root, expected_type)?
                    || self.type_is_fixed_array(root, &lowered.type_hash)?
                    || self.type_is_fixed_array(root, expected_type)?
                    || self.type_is_box(root, &lowered.type_hash)?
                    || self.type_is_box(root, expected_type)?)
                && !self.layouts_blind_copy_compatible(
                    root,
                    ctx.target_triple(),
                    &lowered.type_hash,
                    expected_type,
                )?
            {
                bail!(
                    "unsupported_aggregate_layout: a value of type {} cannot be used where {} is expected because their native layouts differ; bind aggregate literals with an explicit `let x: <Type> = <literal>` so they are built in the destination layout",
                    lowered.type_hash,
                    expected_type
                );
            }
            return Ok(lowered);
        }
        let value = self.lower_expr(root, expr_hash, param_types, ctx, locals)?;
        if value.type_hash != expected_type
            && (self.type_is_record(root, &value.type_hash)?
                || self.type_is_record(root, expected_type)?
                || self.type_is_enum(root, &value.type_hash)?
                || self.type_is_enum(root, expected_type)?
                || self.type_is_fixed_array(root, &value.type_hash)?
                || self.type_is_fixed_array(root, expected_type)?
                || self.type_is_box(root, &value.type_hash)?
                || self.type_is_box(root, expected_type)?)
            && !self.layouts_blind_copy_compatible(
                root,
                ctx.target_triple(),
                &value.type_hash,
                expected_type,
            )?
        {
            bail!(
                "unsupported_aggregate_layout: a value of type {} cannot be used where {} is expected because their native layouts differ; bind aggregate literals with an explicit `let x: <Type> = <literal>` so they are built in the destination layout",
                value.type_hash,
                expected_type
            );
        }
        Ok(value)
    }

    fn type_is_record(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<bool> {
        Ok(matches!(
            self.type_spec_in_root(root, type_hash)?,
            TypeSpec::Record(_)
        ))
    }

    fn type_is_fixed_array(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<bool> {
        Ok(matches!(
            self.type_spec_in_root(root, type_hash)?,
            TypeSpec::FixedArray { .. }
        ))
    }

    fn type_is_enum(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<bool> {
        Ok(matches!(
            self.type_spec_in_root(root, type_hash)?,
            TypeSpec::Enum(_)
        ))
    }

    fn type_is_box(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<bool> {
        Ok(matches!(
            self.type_spec_in_root(root, type_hash)?,
            TypeSpec::Box { .. }
        ))
    }

    /// Whether a verbatim byte copy of a `src`-typed value reinterpreted as
    /// `dst` is sound: identical hashes, layout-equal scalars/references, or
    /// records whose shared field names sit at identical offsets with
    /// recursively-compatible field types and equal total size. Differing field
    /// order (the bug class) yields differing offsets and is rejected.
    fn layouts_blind_copy_compatible(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        src: &str,
        dst: &str,
    ) -> Result<bool> {
        if src == dst {
            return Ok(true);
        }
        match (
            self.type_spec_in_root(root, src)?,
            self.type_spec_in_root(root, dst)?,
        ) {
            (TypeSpec::Record(src_fields), TypeSpec::Record(dst_fields)) => {
                if src_fields.len() != dst_fields.len() {
                    return Ok(false);
                }
                for dst_field in &dst_fields {
                    let Some(src_field) = src_fields
                        .iter()
                        .find(|candidate| candidate.name == dst_field.name)
                    else {
                        return Ok(false);
                    };
                    let src_offset =
                        self.layout_field_offset_bytes(root, target_triple, src, &dst_field.name)?;
                    let dst_offset =
                        self.layout_field_offset_bytes(root, target_triple, dst, &dst_field.name)?;
                    if src_offset != dst_offset {
                        return Ok(false);
                    }
                    if !self.layouts_blind_copy_compatible(
                        root,
                        target_triple,
                        &src_field.type_hash,
                        &dst_field.type_hash,
                    )? {
                        return Ok(false);
                    }
                }
                Ok(self.layout_size_bytes(root, target_triple, src)?
                    == self.layout_size_bytes(root, target_triple, dst)?)
            }
            (TypeSpec::Enum(src_variants), TypeSpec::Enum(dst_variants)) => {
                if src_variants.len() != dst_variants.len() {
                    return Ok(false);
                }
                let src_layout = self.compute_type_layout(root, src, target_triple)?.metadata;
                let dst_layout = self.compute_type_layout(root, dst, target_triple)?.metadata;
                for dst_variant in &dst_variants {
                    let Some(src_variant) = src_variants
                        .iter()
                        .find(|candidate| candidate.name == dst_variant.name)
                    else {
                        return Ok(false);
                    };
                    let src_info = enum_variant_layout(&src_layout, &dst_variant.name)?;
                    let dst_info = enum_variant_layout(&dst_layout, &dst_variant.name)?;
                    if src_info.tag_value != dst_info.tag_value
                        || src_info.payload_offset_bytes != dst_info.payload_offset_bytes
                    {
                        return Ok(false);
                    }
                    if !self.layouts_blind_copy_compatible(
                        root,
                        target_triple,
                        &src_variant.type_hash,
                        &dst_variant.type_hash,
                    )? {
                        return Ok(false);
                    }
                }
                Ok(self.layout_size_bytes(root, target_triple, src)?
                    == self.layout_size_bytes(root, target_triple, dst)?)
            }
            (
                TypeSpec::FixedArray {
                    element: src_element,
                    len: src_len,
                },
                TypeSpec::FixedArray {
                    element: dst_element,
                    len: dst_len,
                },
            ) => {
                if src_len != dst_len {
                    return Ok(false);
                }
                if !self.layouts_blind_copy_compatible(
                    root,
                    target_triple,
                    &src_element,
                    &dst_element,
                )? {
                    return Ok(false);
                }
                Ok(self.layout_size_bytes(root, target_triple, src)?
                    == self.layout_size_bytes(root, target_triple, dst)?)
            }
            (
                TypeSpec::Box {
                    element: src_element,
                },
                TypeSpec::Box {
                    element: dst_element,
                },
            ) => {
                self.layouts_blind_copy_compatible(root, target_triple, &src_element, &dst_element)
            }
            (TypeSpec::Record(_), _)
            | (_, TypeSpec::Record(_))
            | (TypeSpec::Enum(_), _)
            | (_, TypeSpec::Enum(_))
            | (TypeSpec::FixedArray { .. }, _)
            | (_, TypeSpec::FixedArray { .. })
            | (TypeSpec::Box { .. }, _)
            | (_, TypeSpec::Box { .. }) => Ok(false),
            _ => self.scalar_layouts_equal(root, target_triple, src, dst),
        }
    }

    fn scalar_layouts_equal(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        src: &str,
        dst: &str,
    ) -> Result<bool> {
        let src_layout = self.compute_type_layout(root, src, target_triple)?;
        let dst_layout = self.compute_type_layout(root, dst, target_triple)?;
        Ok(
            src_layout.metadata.get("kind") == dst_layout.metadata.get("kind")
                && src_layout.metadata.get("size_bytes") == dst_layout.metadata.get("size_bytes")
                && src_layout.metadata.get("align_bytes") == dst_layout.metadata.get("align_bytes"),
        )
    }

    fn lowered_record_field(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
        field: &str,
    ) -> Result<LoweredFieldInfo> {
        let offset_bytes = self.layout_field_offset_bytes(root, target_triple, type_hash, field)?;
        let field_symbol = if let TypeSpec::Named { type_symbol, .. } = self.type_spec(type_hash)? {
            let entry = self
                .root_type(root, &type_symbol)
                .ok_or_else(|| anyhow!("named record missing from root {type_symbol}"))?;
            let TypeDefinition::Record { fields, .. } = self.type_definition(&entry.type_def)?
            else {
                bail!("field access requires record type");
            };
            fields
                .into_iter()
                .find(|candidate| candidate.name == field)
                .map(|candidate| candidate.member_symbol)
        } else {
            None
        };

        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Record(fields) => fields
                .into_iter()
                .find(|candidate| candidate.name == field)
                .map(|candidate| LoweredFieldInfo {
                    type_hash: candidate.type_hash,
                    field_symbol,
                    offset_bytes,
                })
                .ok_or_else(|| anyhow!("record has no field {field}")),
            other => bail!(
                "field access requires record type, got {}",
                other.to_source(self)?
            ),
        }
    }

    fn lowered_enum_variant(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
        variant: &str,
    ) -> Result<LoweredVariantInfo> {
        let layout = self
            .compute_type_layout(root, type_hash, target_triple)?
            .metadata;
        let layout_variant = enum_variant_layout(&layout, variant)?;
        let variant_symbol = layout_variant.variant_symbol;
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Enum(variants) => {
                let variant_type = variants
                    .into_iter()
                    .find(|candidate| candidate.name == variant)
                    .map(|candidate| candidate.type_hash)
                    .ok_or_else(|| anyhow!("enum has no variant {variant}"))?;
                if layout_variant.type_hash != variant_type {
                    bail!("enum layout variant {variant} type mismatch");
                }
                Ok(LoweredVariantInfo {
                    type_hash: variant_type,
                    variant_symbol,
                    tag_value: layout_variant.tag_value,
                    payload_offset_bytes: layout_variant.payload_offset_bytes,
                })
            }
            other => bail!(
                "enum variant access requires enum type, got {}",
                other.to_source(self)?
            ),
        }
    }

    fn lowered_array_info(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
    ) -> Result<LoweredArrayInfo> {
        let layout = self
            .compute_type_layout(root, type_hash, target_triple)?
            .metadata;
        let layout_element = layout
            .get("element_type_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("array layout missing element_type_hash"))?
            .to_string();
        let layout_len = required_layout_u64(&layout, "len")?;
        let stride_bytes = required_layout_u64(&layout, "stride_bytes")?;
        if stride_bytes == 0 {
            bail!("array layout stride must be non-zero");
        }
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::FixedArray { element, len } => {
                if element != layout_element || len != layout_len {
                    bail!("array layout metadata mismatch");
                }
                Ok(LoweredArrayInfo {
                    element_type_hash: element,
                    len,
                })
            }
            other => bail!(
                "array index requires array type, got {}",
                other.to_source(self)?
            ),
        }
    }

    fn literal_i64_value(&self, expr_hash: &str) -> Result<Option<i64>> {
        let payload = self.get_payload(expr_hash)?;
        if payload.get("expr_kind").and_then(JsonValue::as_str) != Some("literal_i64") {
            return Ok(None);
        }
        Ok(Some(
            payload
                .get("value")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                .parse::<i64>()?,
        ))
    }

    fn aggregate_record_fields(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<Vec<crate::types::TypeFieldSpec>> {
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Record(fields) => Ok(fields),
            other => bail!(
                "aggregate place initializer requires record type, got {}",
                other.to_source(self)?
            ),
        }
    }

    fn expr_is_place(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        Ok(matches!(
            payload.get("expr_kind").and_then(JsonValue::as_str),
            Some("param_ref" | "local_ref" | "field_access" | "array_index")
        ))
    }

    fn layout_size_bytes(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
    ) -> Result<u64> {
        self.compute_type_layout(root, type_hash, target_triple)?
            .metadata
            .get("size_bytes")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("type layout missing size_bytes for {type_hash}"))
    }

    fn type_passes_indirect(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
    ) -> Result<bool> {
        let layout = self.compute_type_layout(root, type_hash, target_triple)?;
        let abi = layout
            .metadata
            .get("abi")
            .ok_or_else(|| anyhow!("type layout missing abi for {type_hash}"))?;
        Ok(required_layout_string(abi, "pass")? == "by_indirect")
    }

    fn type_returns_indirect(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
    ) -> Result<bool> {
        let layout = self.compute_type_layout(root, type_hash, target_triple)?;
        let abi = layout
            .metadata
            .get("abi")
            .ok_or_else(|| anyhow!("type layout missing abi for {type_hash}"))?;
        Ok(required_layout_string(abi, "return")? == "hidden_return_slot")
    }

    fn type_is_move_only(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
    ) -> Result<bool> {
        let layout = self.compute_type_layout(root, type_hash, target_triple)?;
        match layout.metadata.get("copy_kind").and_then(JsonValue::as_str) {
            Some("copy") => Ok(false),
            Some("move_only") => Ok(true),
            Some(other) => bail!("unknown copy_kind {other} for type {type_hash}"),
            None => bail!("type layout missing copy_kind for {type_hash}"),
        }
    }

    fn type_requires_copy_scaffold(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
    ) -> Result<bool> {
        let layout = self.compute_type_layout(root, type_hash, target_triple)?;
        let copy_kind = match layout.metadata.get("copy_kind").and_then(JsonValue::as_str) {
            Some("copy") => true,
            Some("move_only") => false,
            Some(other) => bail!("unknown copy_kind {other} for type {type_hash}"),
            None => bail!("type layout missing copy_kind for {type_hash}"),
        };
        let contains_reference = layout
            .metadata
            .get("contains_reference")
            .and_then(JsonValue::as_bool)
            .ok_or_else(|| anyhow!("type layout missing contains_reference for {type_hash}"))?;
        Ok(copy_kind && contains_reference)
    }

    fn type_requires_drop_scaffold(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
    ) -> Result<bool> {
        let layout = self.compute_type_layout(root, type_hash, target_triple)?;
        let move_only = match layout.metadata.get("copy_kind").and_then(JsonValue::as_str) {
            Some("copy") => false,
            Some("move_only") => true,
            Some(other) => bail!("unknown copy_kind {other} for type {type_hash}"),
            None => bail!("type layout missing copy_kind for {type_hash}"),
        };
        let needs_drop = match layout.metadata.get("drop_kind").and_then(JsonValue::as_str) {
            Some("trivial") => false,
            Some("needs_drop") => true,
            Some(other) => bail!("unknown drop_kind {other} for type {type_hash}"),
            None => bail!("type layout missing drop_kind for {type_hash}"),
        };
        // Copy values own nothing and must not be dropped (SPEC_V2 §12). A
        // shared-reference record is Copy (`contains_reference` true but
        // `copy_kind` "copy"); only move-only or needs-drop values get a drop
        // scaffold.
        Ok(move_only || needs_drop)
    }

    fn layout_field_offset_bytes(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        type_hash: &str,
        field: &str,
    ) -> Result<u64> {
        let layout = self.compute_type_layout(root, type_hash, target_triple)?;
        layout
            .metadata
            .get("fields")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .find(|entry| entry.get("name").and_then(JsonValue::as_str) == Some(field))
            .and_then(|entry| entry.get("offset_bytes").and_then(JsonValue::as_u64))
            .ok_or_else(|| anyhow!("type layout missing offset for field {field}"))
    }

    fn ensure_lowerable_return_type(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<()> {
        let type_name = self.type_name(type_hash)?;
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Builtin(_)
            | TypeSpec::Record(_)
            | TypeSpec::Enum(_)
            | TypeSpec::FixedArray { .. }
            | TypeSpec::Slice { .. }
            | TypeSpec::Reference { .. }
            | TypeSpec::RawPointer { .. }
            | TypeSpec::Box { .. }
            | TypeSpec::Vec { .. }
            | TypeSpec::String => Ok(()),
            _ => bail!("lowering v1 does not support aggregate return type {type_name}"),
        }
    }

    fn ensure_addressable_ir_type(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<()> {
        self.type_spec_in_root(root, type_hash)?;
        Ok(())
    }

    fn is_aggregate_ir_type(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<bool> {
        Ok(matches!(
            self.type_spec_in_root(root, type_hash)?,
            TypeSpec::Record(_)
                | TypeSpec::Enum(_)
                | TypeSpec::FixedArray { .. }
                | TypeSpec::Slice { .. }
                | TypeSpec::Vec { .. }
                | TypeSpec::String
        ))
    }

    fn lowered_type_layouts(
        &self,
        root: &ProgramRootPayload,
        target_triple: &str,
        param_types: &[String],
        return_type: &str,
        local_slots: &[LoweredLocalSlot],
        operations: &[LoweredOp],
    ) -> Result<Vec<LoweredTypeLayout>> {
        let mut type_hashes = BTreeSet::new();
        type_hashes.extend(param_types.iter().cloned());
        type_hashes.insert(return_type.to_string());
        type_hashes.extend(local_slots.iter().map(|local| local.type_hash.clone()));
        collect_op_type_hashes(operations, &mut type_hashes);
        let mut seen_type_hashes = BTreeSet::new();
        for type_hash in type_hashes.clone() {
            self.collect_lowered_layout_type_hashes(
                root,
                &type_hash,
                &mut type_hashes,
                &mut seen_type_hashes,
            )?;
        }

        type_hashes
            .into_iter()
            .map(|type_hash| {
                let layout = self.compute_type_layout(root, &type_hash, target_triple)?;
                let metadata = layout.metadata;
                let abi = metadata
                    .get("abi")
                    .ok_or_else(|| anyhow!("type layout missing abi for {type_hash}"))?;
                Ok(LoweredTypeLayout {
                    type_hash,
                    kind: required_layout_string(&metadata, "kind")?,
                    size_bytes: required_layout_u64(&metadata, "size_bytes")?,
                    align_bytes: required_layout_u64(&metadata, "align_bytes")?,
                    abi: LoweredTypeAbi {
                        pass: required_layout_string(abi, "pass")?,
                        return_: required_layout_string(abi, "return")?,
                    },
                    metadata,
                })
            })
            .collect()
    }

    fn collect_lowered_layout_type_hashes(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        out: &mut BTreeSet<String>,
        seen: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !seen.insert(type_hash.to_string()) {
            return Ok(());
        }
        out.insert(type_hash.to_string());
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Box { element } => {
                self.collect_lowered_layout_type_hashes(root, &element, out, seen)?;
            }
            TypeSpec::Vec { element } => {
                self.collect_lowered_layout_type_hashes(root, &element, out, seen)?;
            }
            TypeSpec::FixedArray { element, .. } | TypeSpec::Slice { element, .. } => {
                self.collect_lowered_layout_type_hashes(root, &element, out, seen)?;
            }
            TypeSpec::Reference { referent, .. } => {
                self.collect_lowered_layout_type_hashes(root, &referent, out, seen)?;
            }
            TypeSpec::RawPointer { pointee, .. } => {
                self.collect_lowered_layout_type_hashes(root, &pointee, out, seen)?;
            }
            TypeSpec::Record(fields) => {
                for field in fields {
                    self.collect_lowered_layout_type_hashes(root, &field.type_hash, out, seen)?;
                }
            }
            TypeSpec::Enum(variants) => {
                for variant in variants {
                    self.collect_lowered_layout_type_hashes(root, &variant.type_hash, out, seen)?;
                }
            }
            TypeSpec::Builtin(_)
            | TypeSpec::Named { .. }
            | TypeSpec::String
            | TypeSpec::TypeParam { .. } => {}
        }
        Ok(())
    }

    pub(crate) fn verify_lowered_ir_against_index(
        &self,
        input_hash: &str,
        target_triple: &str,
        ir: &LoweredFunctionIr,
    ) -> Result<()> {
        if ir.function_def_hash != input_hash {
            bail!(
                "lowered IR input mismatch: cache input {input_hash}, IR input {}",
                ir.function_def_hash
            );
        }
        let root_hashes = self
            .conn
            .prepare(
                "SELECT root_hash FROM root_symbols
                 WHERE definition_hash = ?1 AND symbol_hash = ?2
                 ORDER BY root_hash",
            )?
            .query_map(params![input_hash, &ir.symbol_hash], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if root_hashes.is_empty() {
            return self.verify_lowered_ir_shape(ir);
        }

        let mut last_error = None;
        let layout_target = lowered_ir_layout_target(target_triple);
        for root_hash in root_hashes {
            let root = self.load_root(&root_hash)?;
            let Some(entry) = self.root_symbol(&root, &ir.symbol_hash) else {
                last_error = Some(anyhow!(
                    "lowered IR symbol {} missing from indexed root {root_hash}",
                    ir.symbol_hash
                ));
                continue;
            };
            if let Err(err) = self.verify_lowered_ir(&root, ir, layout_target) {
                last_error = Some(err);
                continue;
            }
            match self.build_lowered_function_ir(&root, entry, layout_target) {
                Ok(expected) if &expected == ir => return Ok(()),
                Ok(_) => {
                    last_error = Some(anyhow!(
                        "lowered IR does not match recomputed semantic DAG for root {root_hash}"
                    ));
                }
                Err(err) => last_error = Some(err),
            }
        }
        if let Some(err) = last_error {
            bail!("{err:#}");
        }
        bail!("lowered IR does not match any indexed root");
    }

    pub(crate) fn verify_lowered_ir(
        &self,
        root: &ProgramRootPayload,
        ir: &LoweredFunctionIr,
        target_triple: &str,
    ) -> Result<()> {
        self.verify_lowered_ir_shape(ir)?;
        let root_entry = self
            .root_symbol(root, &ir.symbol_hash)
            .ok_or_else(|| anyhow!("lowered IR symbol missing from root {}", ir.symbol_hash))?;
        if root_entry.definition != ir.function_def_hash {
            bail!("lowered IR definition does not match root");
        }
        if root_entry.signature != ir.function_sig_hash {
            bail!("lowered IR signature does not match root");
        }

        let (param_types, return_type) = self.signature_parts(&ir.function_sig_hash)?;
        let allowed_regions = self
            .signature_region_params(&ir.function_sig_hash)?
            .into_iter()
            .map(|param| param.region)
            .collect::<BTreeSet<_>>();
        let actual_return = self.verify_expr_type(
            &ir.typed_body_expr_hash,
            root,
            &param_types,
            &allowed_regions,
        )?;
        if !self.type_assignable_in_root(root, &actual_return, &return_type)?
            || ir.return_type_hash != return_type
        {
            bail!("lowered IR return type mismatch");
        }
        self.verify_lowered_operations(root, ir, target_triple, &param_types, &return_type)?;
        let expected_layouts = self.lowered_type_layouts(
            root,
            target_triple,
            &param_types,
            &return_type,
            &ir.locals,
            &ir.operations,
        )?;
        if ir.type_layouts != expected_layouts {
            bail!("lowered IR type layout metadata mismatch");
        }
        Ok(())
    }

    fn verify_lowered_ir_shape(&self, ir: &LoweredFunctionIr) -> Result<()> {
        if ir.schema != LOWERED_IR_SCHEMA {
            bail!("lowered IR schema must be {LOWERED_IR_SCHEMA}");
        }
        if !is_hash(&ir.symbol_hash)
            || !is_hash(&ir.function_def_hash)
            || !is_hash(&ir.function_sig_hash)
            || !is_hash(&ir.typed_body_expr_hash)
            || !is_hash(&ir.return_type_hash)
        {
            bail!("lowered IR contains a non-hash identity field");
        }
        if self.get_kind(&ir.function_def_hash)? != "FunctionDef" {
            bail!("lowered IR function_def_hash is not a FunctionDef");
        }
        let definition = self.get_payload(&ir.function_def_hash)?;
        if definition.get("symbol").and_then(JsonValue::as_str) != Some(&ir.symbol_hash) {
            bail!("lowered IR symbol does not match FunctionDef");
        }
        if definition
            .get("function_sig_hash")
            .and_then(JsonValue::as_str)
            != Some(&ir.function_sig_hash)
        {
            bail!("lowered IR signature does not match FunctionDef");
        }
        if definition
            .get("typed_body_expr_hash")
            .and_then(JsonValue::as_str)
            != Some(&ir.typed_body_expr_hash)
        {
            bail!("lowered IR body does not match FunctionDef");
        }
        let (param_types, return_type) = self.signature_parts(&ir.function_sig_hash)?;
        if ir.return_type_hash != return_type {
            bail!("lowered IR return type does not match signature");
        }
        if ir.params.len() != param_types.len() {
            bail!("lowered IR parameter count mismatch");
        }
        for (slot, (param, expected_type)) in ir.params.iter().zip(param_types.iter()).enumerate() {
            if param.slot != slot || param.type_hash != *expected_type {
                bail!("lowered IR parameter slot {slot} mismatch");
            }
        }
        for (slot, local) in ir.locals.iter().enumerate() {
            if local.slot != slot || !is_hash(&local.type_hash) {
                bail!("lowered IR local slot {slot} mismatch");
            }
            self.type_spec(&local.type_hash)?;
            if local.size_bytes == 0 {
                bail!("lowered IR local slot {slot} has zero size");
            }
        }
        let mut seen_layouts = BTreeSet::new();
        for layout in &ir.type_layouts {
            if !seen_layouts.insert(layout.type_hash.clone()) {
                bail!("duplicate lowered type layout {}", layout.type_hash);
            }
            if !is_hash(&layout.type_hash) {
                bail!("lowered type layout hash is not a hash");
            }
            self.type_spec(&layout.type_hash)?;
            if layout.size_bytes == 0 && layout.kind != "scalar" {
                bail!("lowered non-scalar type layout has zero size");
            }
            if layout.align_bytes == 0 {
                bail!("lowered type layout alignment is zero");
            }
            match (layout.abi.pass.as_str(), layout.abi.return_.as_str()) {
                ("by_value", "by_value") | ("by_indirect", "hidden_return_slot") => {}
                _ => bail!("lowered type layout has unsupported ABI classification"),
            }
        }
        if ir.operations.is_empty() {
            bail!("lowered IR has no return operation");
        }
        self.verify_lowered_debug_map(ir)?;
        Ok(())
    }

    fn verify_lowered_debug_map(&self, ir: &LoweredFunctionIr) -> Result<()> {
        if ir.debug_map.schema != LOWERED_DEBUG_MAP_SCHEMA {
            bail!("lowered debug map schema must be {LOWERED_DEBUG_MAP_SCHEMA}");
        }
        let expected_ops = lowered_value_debug_infos(&ir.operations)?;
        if expected_ops.is_empty() {
            bail!("lowered debug map has no value-producing operations");
        }
        let mut seen_op_ids = BTreeSet::new();
        let mut seen_value_ids = BTreeSet::new();
        let mut actual_ops = BTreeMap::new();
        let mut actual_by_expr: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for op in &ir.debug_map.operations {
            if op.lowered_op_id != lowered_op_id_for_value(&op.value_id) {
                bail!(
                    "lowered debug map op id does not match value id {}",
                    op.value_id
                );
            }
            if !seen_op_ids.insert(op.lowered_op_id.clone()) {
                bail!("duplicate lowered debug op id {}", op.lowered_op_id);
            }
            if !seen_value_ids.insert(op.value_id.clone()) {
                bail!("duplicate lowered debug value id {}", op.value_id);
            }
            if !is_hash(&op.expr_hash) {
                bail!("lowered debug map expression is not a hash");
            }
            if self.get_kind(&op.expr_hash)? != "Expression" {
                bail!("lowered debug map expression is not an Expression");
            }
            actual_by_expr
                .entry(op.expr_hash.clone())
                .or_default()
                .push(op.lowered_op_id.clone());
            actual_ops.insert(
                op.value_id.clone(),
                (op.lowered_op_kind.clone(), op.lowered_op_id.clone()),
            );
        }

        let expected_value_ids = expected_ops.keys().cloned().collect::<BTreeSet<_>>();
        if expected_value_ids != seen_value_ids {
            bail!("lowered debug map does not cover all value-producing operations");
        }
        for (value_id, expected_kind) in expected_ops {
            match actual_ops.get(&value_id) {
                Some((actual_kind, _)) if actual_kind == &expected_kind => {}
                Some((actual_kind, _)) => bail!(
                    "lowered debug map kind {actual_kind} does not match operation kind {expected_kind} for {value_id}"
                ),
                None => bail!("lowered debug map missing value id {value_id}"),
            }
        }

        let expected_expr_to_ops = actual_by_expr
            .into_iter()
            .map(|(expr_hash, lowered_op_ids)| LoweredExprOpMap {
                expr_hash,
                lowered_op_ids,
            })
            .collect::<Vec<_>>();
        if ir.debug_map.expr_to_ops != expected_expr_to_ops {
            bail!("lowered debug map expr_to_ops index mismatch");
        }
        Ok(())
    }

    fn verify_lowered_operations(
        &self,
        root: &ProgramRootPayload,
        ir: &LoweredFunctionIr,
        target_triple: &str,
        param_types: &[String],
        return_type: &str,
    ) -> Result<()> {
        let mut values = BTreeMap::new();
        let mut addresses = BTreeMap::new();
        let mut drop_state = DropTracker::default();
        let (last, body_ops) = ir
            .operations
            .split_last()
            .ok_or_else(|| anyhow!("lowered IR has no operations"))?;
        self.verify_value_ops(
            root,
            body_ops,
            target_triple,
            param_types,
            return_type,
            &ir.locals,
            &mut values,
            &mut addresses,
            &mut drop_state,
        )?;
        match last {
            LoweredOp::Return { value, type_hash } => {
                if type_hash != return_type {
                    bail!("lowered return type does not match function return type");
                }
                let actual = values
                    .get(value)
                    .ok_or_else(|| anyhow!("lowered return references unknown value {value}"))?;
                if !self.type_assignable_in_root(root, actual, type_hash)? {
                    bail!("lowered return value type mismatch");
                }
                Ok(())
            }
            _ => bail!("lowered IR must end with an explicit return operation"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn verify_value_ops(
        &self,
        root: &ProgramRootPayload,
        operations: &[LoweredOp],
        target_triple: &str,
        param_types: &[String],
        return_type: &str,
        local_slots: &[LoweredLocalSlot],
        values: &mut BTreeMap<String, String>,
        addresses: &mut BTreeMap<String, String>,
        drop_state: &mut DropTracker,
    ) -> Result<()> {
        for op in operations {
            match op {
                LoweredOp::Param {
                    id,
                    slot,
                    type_hash,
                } => {
                    let expected = param_types
                        .get(*slot)
                        .ok_or_else(|| anyhow!("lowered param slot out of bounds {slot}"))?;
                    if expected != type_hash {
                        bail!("lowered param slot {slot} type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::ConstI64 {
                    id,
                    value,
                    type_hash,
                } => {
                    // An integer constant of the width given by `type_hash` — i64 or
                    // any sized integer (R5). Lowering normalizes the literal to its
                    // canonical i64 bit pattern, so re-derive that the type is a sized
                    // integer and the bit pattern is within that width's canonical
                    // range (a u64 with the high bit set is a negative i64 pattern).
                    let TypeSpec::Builtin(name) = self.type_spec(type_hash)? else {
                        bail!("lowered const_i64 requires an integer type");
                    };
                    let int = crate::types::scalar_int_type(&name)
                        .ok_or_else(|| anyhow!("lowered const_i64 non-integer type {name}"))?;
                    let bits = int.width * 8;
                    let bit_pattern: i64 = value
                        .parse()
                        .map_err(|_| anyhow!("lowered const_i64 value {value} is not an i64"))?;
                    let in_range = if bits >= 64 {
                        true
                    } else if int.signed {
                        bit_pattern >= -(1i64 << (bits - 1)) && bit_pattern < (1i64 << (bits - 1))
                    } else {
                        bit_pattern >= 0 && bit_pattern < (1i64 << bits)
                    };
                    if !in_range {
                        bail!("lowered const_i64 value {value} out of range for {}", int.name);
                    }
                    insert_value(values, id, type_hash)?;
                    // Only i64 constants index arrays, so only they seed the
                    // constant-index map (used by element-granular drop glue).
                    if int.name == "I64" {
                        drop_state.const_i64.insert(id.clone(), value.parse::<i64>()?);
                    }
                }
                LoweredOp::ConstBool {
                    id,
                    value: _,
                    type_hash,
                } => {
                    if type_hash != &type_hash_for("Bool") {
                        bail!("lowered const_bool type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::ConstUnit { id, type_hash } => {
                    if type_hash != &type_hash_for("Unit") {
                        bail!("lowered const_unit type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Unary {
                    id,
                    kind,
                    value,
                    type_hash,
                } => {
                    let value_type = value_type(values, value)?;
                    verify_unary_kind(kind, value_type, type_hash)?;
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::IntCast {
                    id,
                    value,
                    type_hash,
                } => {
                    let value_type = value_type(values, value)?;
                    if crate::types::scalar_int_type_by_hash(value_type).is_none() {
                        bail!("lowered int_cast operand is not a sized integer");
                    }
                    if crate::types::scalar_int_type_by_hash(type_hash).is_none() {
                        bail!("lowered int_cast target is not a sized integer");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Binary {
                    id,
                    kind,
                    left,
                    right,
                    type_hash,
                    trap,
                } => {
                    let left_type = value_type(values, left)?;
                    let right_type = value_type(values, right)?;
                    verify_binary_kind(kind, left_type, right_type, type_hash, trap.as_ref())?;
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Call {
                    id,
                    target_symbol_hash,
                    target_abi_symbol,
                    args,
                    return_address,
                    type_hash,
                } => {
                    if !is_hash(target_symbol_hash) {
                        bail!("lowered call target is not a symbol hash");
                    }
                    let target = self.root_symbol(root, target_symbol_hash).ok_or_else(|| {
                        anyhow!("lowered call target missing {target_symbol_hash}")
                    })?;
                    let (expected_args, expected_return) =
                        self.signature_parts(&target.signature)?;
                    let callee_regions = self
                        .signature_region_params(&target.signature)?
                        .into_iter()
                        .map(|param| param.region)
                        .collect::<BTreeSet<_>>();
                    let mut region_substitutions = BTreeMap::new();
                    if args.len() != expected_args.len() {
                        bail!("lowered call arity mismatch for {target_symbol_hash}");
                    }
                    for (idx, arg) in args.iter().enumerate() {
                        let actual = value_type(values, arg)?;
                        if !self.type_assignable_for_call_in_root(
                            root,
                            actual,
                            &expected_args[idx],
                            &callee_regions,
                        )? {
                            bail!("lowered call argument {idx} type mismatch");
                        }
                        self.infer_call_region_substitutions_for_types(
                            root,
                            actual,
                            &expected_args[idx],
                            &callee_regions,
                            &mut region_substitutions,
                        )?;
                    }
                    let expected_return = self.substitute_type_regions_hash_for_verify(
                        &expected_return,
                        &region_substitutions,
                    )?;
                    if type_hash != &expected_return {
                        bail!("lowered call return type mismatch");
                    }
                    if self.type_returns_indirect(root, target_triple, type_hash)? {
                        let Some(return_address) = return_address else {
                            bail!("lowered aggregate call missing return address");
                        };
                        if address_type(addresses, return_address)? != type_hash {
                            bail!("lowered aggregate call return address type mismatch");
                        }
                    } else if return_address.is_some() {
                        bail!("lowered scalar call must not have return address");
                    }
                    let expected_abi_symbol = if self.definition_is_external(&target.definition)? {
                        Some(
                            self.external_function_metadata(&target.definition)?
                                .link_name,
                        )
                    } else {
                        None
                    };
                    if target_abi_symbol != &expected_abi_symbol {
                        bail!("lowered call ABI symbol does not match target definition");
                    }
                    insert_value(values, id, type_hash)?;
                    if self.type_returns_indirect(root, target_triple, type_hash)? {
                        insert_address(addresses, id, type_hash)?;
                    }
                }
                LoweredOp::If {
                    id,
                    cond,
                    then_block,
                    else_block,
                    type_hash,
                } => {
                    if value_type(values, cond)? != &type_hash_for("Bool") {
                        bail!("lowered if condition must be bool");
                    }
                    // Conditional drop glue (SPEC_V3 §7): verify each branch with
                    // isolated move/drop state (so one branch's compensating drop
                    // is not mistaken for a conflict with the other's move), then
                    // merge — a place consumed (moved or dropped) on every branch
                    // is dead after the `if`. Address/value maps are branch-local.
                    let moved_before = drop_state.moved.clone();
                    let dropped_before = drop_state.dropped.clone();
                    // Early exit (R7): a branch that always `return`s exits through
                    // its own `EarlyReturn` (which already dropped its live values)
                    // and never reaches the merge — so it yields the return type
                    // (exempt from the result-type check) and is excluded from the
                    // consumed-merge; only the non-divergent branch(es) flow into the
                    // continuation.
                    let then_div = lowered_block_diverges(then_block);
                    let else_div = lowered_block_diverges(else_block);
                    let then_type = self.verify_lowered_block(
                        root,
                        then_block,
                        target_triple,
                        param_types,
                        return_type,
                        local_slots,
                        values,
                        addresses,
                        drop_state,
                    )?;
                    let then_consumed =
                        newly_consumed_places(drop_state, &moved_before, &dropped_before);
                    drop_state.moved = moved_before.clone();
                    drop_state.dropped = dropped_before.clone();
                    let else_type = self.verify_lowered_block(
                        root,
                        else_block,
                        target_triple,
                        param_types,
                        return_type,
                        local_slots,
                        values,
                        addresses,
                        drop_state,
                    )?;
                    let else_consumed =
                        newly_consumed_places(drop_state, &moved_before, &dropped_before);
                    if !then_div && then_type != *type_hash {
                        bail!("lowered if branch type mismatch");
                    }
                    if !else_div && else_type != *type_hash {
                        bail!("lowered if branch type mismatch");
                    }
                    drop_state.moved = moved_before;
                    drop_state.dropped = dropped_before;
                    let mut consumed = Vec::new();
                    if !then_div {
                        consumed.push(then_consumed);
                    }
                    if !else_div {
                        consumed.push(else_consumed);
                    }
                    merge_consumed_into_moved(drop_state, &consumed);
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Case {
                    id,
                    scrutinee,
                    enum_type_hash,
                    arms,
                    type_hash,
                } => {
                    if value_type(values, scrutinee)? != enum_type_hash {
                        bail!("lowered case scrutinee type mismatch");
                    }
                    if self.type_passes_indirect(root, target_triple, enum_type_hash)?
                        && address_type(addresses, scrutinee)? != enum_type_hash
                    {
                        bail!("lowered case scrutinee address type mismatch");
                    }
                    let TypeSpec::Enum(variants) = self.type_spec_in_root(root, enum_type_hash)?
                    else {
                        bail!("lowered case requires enum scrutinee");
                    };
                    // Conditional drop glue (SPEC_V3 §7): isolate each arm's
                    // move/drop state and merge — a place consumed on every arm is
                    // dead after the `case`.
                    let moved_before = drop_state.moved.clone();
                    let dropped_before = drop_state.dropped.clone();
                    let mut arm_consumed: Vec<BTreeSet<MovedPlace>> = Vec::with_capacity(arms.len());
                    let mut seen = BTreeSet::new();
                    for arm in arms {
                        if !seen.insert(arm.variant.clone()) {
                            bail!("duplicate lowered case arm {}", arm.variant);
                        }
                        let variant = variants
                            .iter()
                            .find(|candidate| candidate.name == arm.variant)
                            .ok_or_else(|| {
                                anyhow!("lowered case arm uses unknown variant {}", arm.variant)
                            })?;
                        if variant.type_hash != arm.payload_type_hash {
                            bail!("lowered case arm payload type mismatch");
                        }
                        let layout_variant = self.lowered_enum_variant(
                            root,
                            target_triple,
                            enum_type_hash,
                            &arm.variant,
                        )?;
                        if layout_variant.variant_symbol != arm.variant_symbol
                            || layout_variant.tag_value != arm.tag_value
                            || layout_variant.payload_offset_bytes != arm.payload_offset_bytes
                            || layout_variant.type_hash != arm.payload_type_hash
                        {
                            bail!("lowered case arm layout metadata mismatch");
                        }
                        drop_state.moved = moved_before.clone();
                        drop_state.dropped = dropped_before.clone();
                        let arm_type = self.verify_lowered_block(
                            root,
                            &arm.block,
                            target_triple,
                            param_types,
                            return_type,
                            local_slots,
                            values,
                            addresses,
                            drop_state,
                        )?;
                        // Early exit (R7): a divergent arm yields the return type and
                        // exits, so it is exempt from the result-type check and from
                        // the consumed-merge (it never reaches the continuation).
                        let arm_div = lowered_block_diverges(&arm.block);
                        if !arm_div && arm_type != *type_hash {
                            bail!("lowered case arm result type mismatch");
                        }
                        if !arm_div {
                            arm_consumed.push(newly_consumed_places(
                                drop_state,
                                &moved_before,
                                &dropped_before,
                            ));
                        }
                    }
                    let expected = variants
                        .iter()
                        .map(|variant| variant.name.clone())
                        .collect::<BTreeSet<_>>();
                    if seen != expected {
                        bail!("lowered case must cover every enum variant");
                    }
                    drop_state.moved = moved_before;
                    drop_state.dropped = dropped_before;
                    merge_consumed_into_moved(drop_state, &arm_consumed);
                    insert_value(values, id, type_hash)?;
                    if self.type_passes_indirect(root, target_triple, type_hash)? {
                        insert_address(addresses, id, type_hash)?;
                    }
                }
                LoweredOp::Fold {
                    id,
                    target_address,
                    target_type_hash,
                    len,
                    init,
                    index_slot,
                    acc_slot,
                    item_slot,
                    body,
                    element_type_hash,
                    acc_type_hash,
                    type_hash,
                } => {
                    if type_hash != acc_type_hash {
                        bail!("lowered fold result/accumulator type mismatch");
                    }
                    if address_type(addresses, target_address)? != target_type_hash {
                        bail!("lowered fold target address type mismatch");
                    }
                    if value_type(values, len)? != &type_hash_for("I64") {
                        bail!("lowered fold len must be i64");
                    }
                    if value_type(values, init)? != acc_type_hash {
                        bail!("lowered fold init type mismatch");
                    }
                    let index_slot_type = local_slots
                        .get(*index_slot)
                        .ok_or_else(|| anyhow!("lowered fold index slot out of bounds"))?
                        .type_hash
                        .clone();
                    let item_slot_type = local_slots
                        .get(*item_slot)
                        .ok_or_else(|| anyhow!("lowered fold item slot out of bounds"))?
                        .type_hash
                        .clone();
                    let acc_slot_type = local_slots
                        .get(*acc_slot)
                        .ok_or_else(|| anyhow!("lowered fold accumulator slot out of bounds"))?
                        .type_hash
                        .clone();
                    if index_slot_type != type_hash_for("I64")
                        || item_slot_type != *element_type_hash
                        || acc_slot_type != *acc_type_hash
                    {
                        bail!("lowered fold local slot type mismatch");
                    }
                    match self.type_spec_in_root(root, target_type_hash)? {
                        TypeSpec::FixedArray { element, .. } if element == *element_type_hash => {}
                        TypeSpec::Slice { element, .. } if element == *element_type_hash => {}
                        _ => bail!("lowered fold target element type mismatch"),
                    }
                    let body_type = self.verify_lowered_block(
                        root,
                        body,
                        target_triple,
                        param_types,
                        return_type,
                        local_slots,
                        values,
                        addresses,
                        drop_state,
                    )?;
                    if body_type != *acc_type_hash {
                        bail!("lowered fold body result type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                    if self.type_passes_indirect(root, target_triple, type_hash)? {
                        insert_address(addresses, id, type_hash)?;
                    }
                }
                LoweredOp::Loop {
                    id,
                    acc_slot,
                    init,
                    cond,
                    body,
                    acc_type_hash,
                    type_hash,
                } => {
                    if type_hash != acc_type_hash {
                        bail!("lowered loop result/accumulator type mismatch");
                    }
                    if value_type(values, init)? != acc_type_hash {
                        bail!("lowered loop init type mismatch");
                    }
                    let acc_slot_type = local_slots
                        .get(*acc_slot)
                        .ok_or_else(|| anyhow!("lowered loop accumulator slot out of bounds"))?
                        .type_hash
                        .clone();
                    if acc_slot_type != *acc_type_hash {
                        bail!("lowered loop accumulator slot type mismatch");
                    }
                    // `cond` and `body` read the accumulator slot and run 0..N times;
                    // since the body moves no owned values (enforced at lowering),
                    // the shared drop-state is unchanged across the back-edge.
                    let cond_type = self.verify_lowered_block(
                        root,
                        cond,
                        target_triple,
                        param_types,
                        return_type,
                        local_slots,
                        values,
                        addresses,
                        drop_state,
                    )?;
                    if cond_type != type_hash_for("Bool") {
                        bail!("lowered loop condition must be bool");
                    }
                    let body_type = self.verify_lowered_block(
                        root,
                        body,
                        target_triple,
                        param_types,
                        return_type,
                        local_slots,
                        values,
                        addresses,
                        drop_state,
                    )?;
                    if body_type != *acc_type_hash {
                        bail!("lowered loop body result type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                    if self.type_passes_indirect(root, target_triple, type_hash)? {
                        insert_address(addresses, id, type_hash)?;
                    }
                }
                LoweredOp::BorrowShared {
                    id,
                    address,
                    region,
                    referent_type_hash,
                    type_hash,
                } => {
                    if !is_hash(region) {
                        bail!("lowered borrow_shared region is not a hash");
                    }
                    if address_type(addresses, address)? != referent_type_hash {
                        bail!("lowered borrow_shared referent type mismatch");
                    }
                    match self.type_spec(type_hash)? {
                        TypeSpec::Reference {
                            region: actual_region,
                            mutable: false,
                            referent,
                        } if actual_region == *region && referent == *referent_type_hash => {}
                        _ => bail!("lowered borrow_shared type mismatch"),
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::BorrowMut {
                    id,
                    address,
                    region,
                    referent_type_hash,
                    type_hash,
                } => {
                    if !is_hash(region) {
                        bail!("lowered borrow_mut region is not a hash");
                    }
                    if address_type(addresses, address)? != referent_type_hash {
                        bail!("lowered borrow_mut referent type mismatch");
                    }
                    match self.type_spec(type_hash)? {
                        TypeSpec::Reference {
                            region: actual_region,
                            mutable: true,
                            referent,
                        } if actual_region == *region && referent == *referent_type_hash => {}
                        _ => bail!("lowered borrow_mut type mismatch"),
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::DerefShared {
                    id,
                    reference,
                    referent_type_hash,
                } => {
                    match self.type_spec(value_type(values, reference)?)? {
                        TypeSpec::Reference {
                            mutable: false,
                            referent,
                            ..
                        } if referent == *referent_type_hash => {}
                        _ => bail!("lowered deref_shared requires shared reference value"),
                    }
                    insert_address(addresses, id, referent_type_hash)?;
                    if self.is_aggregate_ir_type(root, referent_type_hash)? {
                        insert_value(values, id, referent_type_hash)?;
                    }
                }
                LoweredOp::DerefMut {
                    id,
                    reference,
                    referent_type_hash,
                } => {
                    match self.type_spec(value_type(values, reference)?)? {
                        TypeSpec::Reference {
                            mutable: true,
                            referent,
                            ..
                        } if referent == *referent_type_hash => {}
                        _ => bail!("lowered deref_mut requires mutable reference value"),
                    }
                    insert_address(addresses, id, referent_type_hash)?;
                    if self.is_aggregate_ir_type(root, referent_type_hash)? {
                        insert_value(values, id, referent_type_hash)?;
                    }
                }
                LoweredOp::DerefBox {
                    id,
                    box_value,
                    box_type_hash,
                    element_type_hash,
                } => {
                    match self.type_spec(value_type(values, box_value)?)? {
                        TypeSpec::Box { element } if element == *element_type_hash => {}
                        _ => bail!("lowered deref_box requires box value"),
                    }
                    if self.type_spec(box_type_hash)?
                        != (TypeSpec::Box {
                            element: element_type_hash.clone(),
                        })
                    {
                        bail!("lowered deref_box box type mismatch");
                    }
                    insert_address(addresses, id, element_type_hash)?;
                    // If the box value was loaded out of a tracked place, the pointee
                    // address is that place plus a `Deref` step — so a field/element
                    // move/drop reached through this box deref resolves to a tracked
                    // sub-place (field-granular drop glue through heap indirection;
                    // SPEC_V3 §7). An untracked box value (e.g. from `box_new` or a
                    // call) leaves the pointee untracked, so a partial move through it
                    // fails closed at the `Move` check below — sound, just stricter.
                    if let Some(place) = drop_state.loaded_box_place.get(box_value).cloned() {
                        let mut deref_place = place;
                        deref_place.path.push(PlaceStep::Deref);
                        drop_state.addr_places.insert(id.clone(), deref_place);
                    }
                    if self.is_aggregate_ir_type(root, element_type_hash)? {
                        insert_value(values, id, element_type_hash)?;
                    }
                }
                LoweredOp::UnboxMove {
                    id,
                    box_value,
                    box_type_hash,
                    element_type_hash,
                    dest_slot,
                } => {
                    match self.type_spec(value_type(values, box_value)?)? {
                        TypeSpec::Box { element } if element == *element_type_hash => {}
                        _ => bail!("lowered unbox_move requires box value"),
                    }
                    if self.type_spec(box_type_hash)?
                        != (TypeSpec::Box {
                            element: element_type_hash.clone(),
                        })
                    {
                        bail!("lowered unbox_move box type mismatch");
                    }
                    // The owned scratch slot must exist and match the payload type.
                    let expected = local_slots.get(*dest_slot).ok_or_else(|| {
                        anyhow!("lowered unbox_move dest slot out of bounds {dest_slot}")
                    })?;
                    if expected.slot != *dest_slot || expected.type_hash != *element_type_hash {
                        bail!("lowered unbox_move dest slot type mismatch");
                    }
                    // The result is an owned rvalue of the payload type. Aggregates are
                    // represented as a pointer to their backing slot, so register the
                    // result as an address too (consumers memcpy from it); it is NOT a
                    // drop-tracked place (the scratch slot's ownership flows out).
                    insert_value(values, id, element_type_hash)?;
                    if self.is_aggregate_ir_type(root, element_type_hash)? {
                        insert_address(addresses, id, element_type_hash)?;
                    }
                }
                LoweredOp::HeapAlloc {
                    id,
                    size_bytes,
                    align_bytes,
                    element_type_hash,
                    type_hash,
                } => {
                    if *size_bytes == 0 || *align_bytes == 0 {
                        bail!("lowered heap_alloc must have nonzero size and align");
                    }
                    let element_layout =
                        self.compute_type_layout(root, element_type_hash, target_triple)?;
                    if element_layout
                        .metadata
                        .get("size_bytes")
                        .and_then(JsonValue::as_u64)
                        != Some(*size_bytes)
                        || element_layout
                            .metadata
                            .get("align_bytes")
                            .and_then(JsonValue::as_u64)
                            != Some(*align_bytes)
                    {
                        bail!("lowered heap_alloc element layout mismatch");
                    }
                    match self.type_spec(type_hash)? {
                        TypeSpec::Box { element } if element == *element_type_hash => {}
                        _ => bail!("lowered heap_alloc type must be matching box"),
                    }
                    insert_value(values, id, type_hash)?;
                    insert_address(addresses, id, element_type_hash)?;
                }
                LoweredOp::PtrCast {
                    id,
                    value,
                    source_type_hash,
                    type_hash,
                } => {
                    if value_type(values, value)? != source_type_hash {
                        bail!("lowered ptr_cast source type mismatch");
                    }
                    let TypeSpec::RawPointer { mutable, pointee } = self.type_spec(type_hash)?
                    else {
                        bail!("lowered ptr_cast result must be raw pointer");
                    };
                    match self.type_spec(source_type_hash)? {
                        TypeSpec::Reference {
                            mutable: source_mutable,
                            referent,
                            ..
                        } if referent == pointee && (!mutable || source_mutable) => {}
                        TypeSpec::RawPointer {
                            mutable: source_mutable,
                            pointee: source_pointee,
                        } if source_pointee == pointee && (!mutable || source_mutable) => {}
                        _ => bail!("lowered ptr_cast source must be compatible pointer"),
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::DerefRaw {
                    id,
                    pointer,
                    pointer_type_hash,
                    pointee_type_hash,
                    mutable,
                } => {
                    if value_type(values, pointer)? != pointer_type_hash {
                        bail!("lowered deref_raw pointer type mismatch");
                    }
                    match self.type_spec(pointer_type_hash)? {
                        TypeSpec::RawPointer {
                            mutable: source_mutable,
                            pointee,
                        } if pointee == *pointee_type_hash && (!*mutable || source_mutable) => {}
                        _ => bail!("lowered deref_raw requires compatible raw pointer value"),
                    }
                    insert_address(addresses, id, pointee_type_hash)?;
                    if self.is_aggregate_ir_type(root, pointee_type_hash)? {
                        insert_value(values, id, pointee_type_hash)?;
                    }
                }
                LoweredOp::AddrOfParam { id, place } => {
                    let LoweredPlace::Param {
                        slot,
                        type_hash,
                        indirect,
                    } = place
                    else {
                        bail!("addr_of_param must contain a param place");
                    };
                    let expected = param_types.get(*slot).ok_or_else(|| {
                        anyhow!("lowered addr_of_param slot out of bounds {slot}")
                    })?;
                    if expected != type_hash {
                        bail!("lowered addr_of_param slot {slot} type mismatch");
                    }
                    if *indirect != self.type_passes_indirect(root, target_triple, type_hash)? {
                        bail!("lowered addr_of_param indirect flag mismatch");
                    }
                    insert_address(addresses, id, type_hash)?;
                    drop_state
                        .addr_places
                        .insert(id.clone(), MovedPlace::whole(RootSlot::Param(*slot)));
                    if self.type_passes_indirect(root, target_triple, type_hash)? {
                        insert_value(values, id, type_hash)?;
                    }
                }
                LoweredOp::AddrOfLocal { id, place } => {
                    let LoweredPlace::Local { slot, type_hash } = place else {
                        bail!("addr_of_local must contain a local place");
                    };
                    let expected = local_slots.get(*slot).ok_or_else(|| {
                        anyhow!("lowered addr_of_local slot out of bounds {slot}")
                    })?;
                    if expected.slot != *slot || expected.type_hash != *type_hash {
                        bail!("lowered addr_of_local slot {slot} type mismatch");
                    }
                    insert_address(addresses, id, type_hash)?;
                    drop_state
                        .addr_places
                        .insert(id.clone(), MovedPlace::whole(RootSlot::Local(*slot)));
                    if self.is_aggregate_ir_type(root, type_hash)? {
                        insert_value(values, id, type_hash)?;
                    }
                }
                LoweredOp::AddrOfField { id, place } => {
                    let LoweredPlace::Field {
                        base,
                        field,
                        field_symbol,
                        owner_type_hash,
                        offset_bytes,
                        type_hash,
                    } = place
                    else {
                        bail!("addr_of_field must contain a field place");
                    };
                    let base_type = address_type(addresses, base)?;
                    if base_type != owner_type_hash {
                        bail!("lowered addr_of_field owner type mismatch");
                    }
                    let field_info =
                        self.lowered_record_field(root, target_triple, owner_type_hash, field)?;
                    if field_info.type_hash != *type_hash
                        && !self.type_assignable_in_root(root, type_hash, &field_info.type_hash)?
                    {
                        bail!("lowered addr_of_field type mismatch");
                    }
                    if &field_info.field_symbol != field_symbol {
                        bail!("lowered addr_of_field symbol mismatch");
                    }
                    if field_info.offset_bytes != *offset_bytes {
                        bail!("lowered addr_of_field offset mismatch");
                    }
                    insert_address(addresses, id, type_hash)?;
                    // Extend the field-path of the base place (if it is a tracked
                    // slot/field chain) so field-granular moves and drops resolve
                    // to the right sub-place (SPEC_V3 §7).
                    if let Some(base_place) = drop_state.addr_places.get(base).cloned() {
                        let mut field_place = base_place;
                        field_place.path.push(PlaceStep::Field(field.clone()));
                        drop_state.addr_places.insert(id.clone(), field_place);
                    }
                    if self.is_aggregate_ir_type(root, type_hash)? {
                        insert_value(values, id, type_hash)?;
                    }
                }
                LoweredOp::AddrOfEnumPayload { id, place } => {
                    let LoweredPlace::EnumPayload {
                        base,
                        variant,
                        variant_symbol,
                        owner_type_hash,
                        tag_value,
                        payload_offset_bytes,
                        type_hash,
                    } = place
                    else {
                        bail!("addr_of_enum_payload must contain an enum payload place");
                    };
                    let base_type = address_type(addresses, base)?;
                    if base_type != owner_type_hash {
                        bail!("lowered addr_of_enum_payload owner type mismatch");
                    }
                    let variant_info =
                        self.lowered_enum_variant(root, target_triple, owner_type_hash, variant)?;
                    if variant_info.type_hash != *type_hash
                        || variant_info.variant_symbol != *variant_symbol
                        || variant_info.tag_value != *tag_value
                        || variant_info.payload_offset_bytes != *payload_offset_bytes
                    {
                        bail!("lowered addr_of_enum_payload metadata mismatch");
                    }
                    insert_address(addresses, id, type_hash)?;
                    drop_state
                        .enum_payload_addr
                        .insert(id.clone(), (base.clone(), variant.clone()));
                    if self.is_aggregate_ir_type(root, type_hash)? {
                        insert_value(values, id, type_hash)?;
                    }
                }
                LoweredOp::AddrOfIndex { id, place } => {
                    let LoweredPlace::Index {
                        base,
                        index,
                        element_type_hash,
                        type_hash,
                    } = place
                    else {
                        bail!("addr_of_index must contain an index place");
                    };
                    let base_type = address_type(addresses, base)?;
                    if value_type(values, index)? != &type_hash_for("I64") {
                        bail!("lowered addr_of_index index must be i64");
                    }
                    if base_type == element_type_hash {
                        if element_type_hash != type_hash {
                            bail!("lowered addr_of_index slice element type mismatch");
                        }
                    } else {
                        match self.type_spec_in_root(root, base_type)? {
                            TypeSpec::FixedArray { element, .. } => {
                                if element != *element_type_hash || element != *type_hash {
                                    bail!("lowered addr_of_index element type mismatch");
                                }
                            }
                            other => bail!(
                                "lowered addr_of_index requires array or slice element base, got {}",
                                other.to_source(self)?
                            ),
                        }
                    }
                    insert_address(addresses, id, type_hash)?;
                    // A constant array index extends the base's tracked place with an
                    // `Index` step, so element-granular moves/drops resolve to the right
                    // element (SPEC_V3 §7). A dynamic index is left untracked — a partial
                    // move through one then fails closed at the `Move` check below; a
                    // slice/data base is not in `addr_places`, so it is untracked too.
                    if let Some(base_place) = drop_state.addr_places.get(base).cloned()
                        && let Some(&const_index) = drop_state.const_i64.get(index)
                        && const_index >= 0
                    {
                        let mut element_place = base_place;
                        element_place.path.push(PlaceStep::Index(const_index as u64));
                        drop_state.addr_places.insert(id.clone(), element_place);
                    }
                    if self.is_aggregate_ir_type(root, type_hash)? {
                        insert_value(values, id, type_hash)?;
                    }
                }
                LoweredOp::StaticDataAddress {
                    id,
                    static_data_hash,
                    bytes_hex,
                    len,
                    element_type_hash,
                } => {
                    if element_type_hash != &type_hash_for("U8") {
                        bail!("lowered static_data_address element type must be u8");
                    }
                    let expected = self.static_data_bytes_hex(static_data_hash)?;
                    if &expected != bytes_hex {
                        bail!("lowered static_data_address bytes mismatch");
                    }
                    if bytes_hex.len() / 2 != usize::try_from(*len)? {
                        bail!("lowered static_data_address len mismatch");
                    }
                    insert_address(addresses, id, element_type_hash)?;
                }
                LoweredOp::ConstructSlice {
                    id,
                    address,
                    data_address,
                    len,
                    element_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered construct_slice address type mismatch");
                    }
                    let data_type = address_type(addresses, data_address)?;
                    if data_type != element_type_hash {
                        match self.type_spec_in_root(root, data_type)? {
                            TypeSpec::FixedArray { element, .. }
                                if element == *element_type_hash => {}
                            _ => bail!("lowered construct_slice data address type mismatch"),
                        }
                    }
                    if value_type(values, len)? != &type_hash_for("I64") {
                        bail!("lowered construct_slice len must be i64");
                    }
                    match self.type_spec(type_hash)? {
                        TypeSpec::Slice { element, .. } if element == *element_type_hash => {}
                        _ => bail!("lowered construct_slice type mismatch"),
                    }
                    insert_value(values, id, type_hash)?;
                    insert_address(addresses, id, type_hash)?;
                }
                LoweredOp::SliceLen {
                    id,
                    slice,
                    slice_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, slice)? != slice_type_hash {
                        bail!("lowered slice_len address type mismatch");
                    }
                    if !matches!(self.type_spec(slice_type_hash)?, TypeSpec::Slice { .. }) {
                        bail!("lowered slice_len target must be slice");
                    }
                    if type_hash != &type_hash_for("I64") {
                        bail!("lowered slice_len result must be i64");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::SliceData {
                    id,
                    slice,
                    slice_type_hash,
                    element_type_hash,
                } => {
                    if address_type(addresses, slice)? != slice_type_hash {
                        bail!("lowered slice_data address type mismatch");
                    }
                    match self.type_spec(slice_type_hash)? {
                        TypeSpec::Slice { element, .. } if element == *element_type_hash => {}
                        _ => bail!("lowered slice_data type mismatch"),
                    }
                    insert_address(addresses, id, element_type_hash)?;
                }
                LoweredOp::VecNew {
                    id,
                    address,
                    capacity: _,
                    element_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered vec_new address type mismatch");
                    }
                    match self.type_spec(type_hash)? {
                        TypeSpec::Vec { element } if element == *element_type_hash => {}
                        _ => bail!("lowered vec_new type mismatch"),
                    }
                    self.compute_type_layout(root, element_type_hash, target_triple)?;
                    insert_value(values, id, type_hash)?;
                    insert_address(addresses, id, type_hash)?;
                }
                LoweredOp::VecPush {
                    id,
                    vec_address,
                    value,
                    vec_type_hash,
                    element_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, vec_address)? != vec_type_hash {
                        bail!("lowered vec_push address type mismatch");
                    }
                    match self.type_spec(vec_type_hash)? {
                        TypeSpec::Vec { element } if element == *element_type_hash => {}
                        _ => bail!("lowered vec_push vec type mismatch"),
                    }
                    if value_type(values, value)? != element_type_hash {
                        bail!("lowered vec_push value type mismatch");
                    }
                    if type_hash != &type_hash_for("Unit") {
                        bail!("lowered vec_push result type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::VecGet {
                    id,
                    vec_address,
                    index,
                    vec_type_hash,
                    element_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, vec_address)? != vec_type_hash {
                        bail!("lowered vec_get address type mismatch");
                    }
                    match self.type_spec(vec_type_hash)? {
                        TypeSpec::Vec { element } if element == *element_type_hash => {}
                        _ => bail!("lowered vec_get vec type mismatch"),
                    }
                    if value_type(values, index)? != &type_hash_for("I64") {
                        bail!("lowered vec_get index must be i64");
                    }
                    if type_hash != element_type_hash {
                        bail!("lowered vec_get result type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                    if self.is_aggregate_ir_type(root, type_hash)? {
                        insert_address(addresses, id, type_hash)?;
                    }
                }
                LoweredOp::VecLen {
                    id,
                    vec_address,
                    vec_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, vec_address)? != vec_type_hash {
                        bail!("lowered vec_len address type mismatch");
                    }
                    if !matches!(self.type_spec(vec_type_hash)?, TypeSpec::Vec { .. }) {
                        bail!("lowered vec_len target must be vec");
                    }
                    if type_hash != &type_hash_for("I64") {
                        bail!("lowered vec_len result must be i64");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::StringNew {
                    id,
                    address,
                    static_data_hash,
                    bytes_hex,
                    len,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered string_new address type mismatch");
                    }
                    if !matches!(self.type_spec(type_hash)?, TypeSpec::String) {
                        bail!("lowered string_new type mismatch");
                    }
                    let expected = self.static_data_bytes_hex(static_data_hash)?;
                    if &expected != bytes_hex {
                        bail!("lowered string_new bytes mismatch");
                    }
                    if bytes_hex.len() / 2 != usize::try_from(*len)? {
                        bail!("lowered string_new len mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                    insert_address(addresses, id, type_hash)?;
                }
                LoweredOp::StringLen {
                    id,
                    string_address,
                    string_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, string_address)? != string_type_hash {
                        bail!("lowered string_len address type mismatch");
                    }
                    if !matches!(self.type_spec(string_type_hash)?, TypeSpec::String) {
                        bail!("lowered string_len target must be string");
                    }
                    if type_hash != &type_hash_for("I64") {
                        bail!("lowered string_len result must be i64");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::StringWithCapacity {
                    id,
                    address,
                    capacity,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered string_with_capacity address type mismatch");
                    }
                    if !matches!(self.type_spec(type_hash)?, TypeSpec::String) {
                        bail!("lowered string_with_capacity type mismatch");
                    }
                    if value_type(values, capacity)? != &type_hash_for("I64") {
                        bail!("lowered string_with_capacity capacity must be i64");
                    }
                    insert_value(values, id, type_hash)?;
                    insert_address(addresses, id, type_hash)?;
                }
                LoweredOp::StringPush {
                    id,
                    string_address,
                    value,
                    string_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, string_address)? != string_type_hash {
                        bail!("lowered string_push address type mismatch");
                    }
                    if !matches!(self.type_spec(string_type_hash)?, TypeSpec::String) {
                        bail!("lowered string_push target must be string");
                    }
                    if value_type(values, value)? != &type_hash_for("U8") {
                        bail!("lowered string_push value must be u8");
                    }
                    if type_hash != &type_hash_for("Unit") {
                        bail!("lowered string_push result type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::StringGet {
                    id,
                    string_address,
                    index,
                    string_type_hash,
                    type_hash,
                } => {
                    if address_type(addresses, string_address)? != string_type_hash {
                        bail!("lowered string_get address type mismatch");
                    }
                    if !matches!(self.type_spec(string_type_hash)?, TypeSpec::String) {
                        bail!("lowered string_get target must be string");
                    }
                    if value_type(values, index)? != &type_hash_for("I64") {
                        bail!("lowered string_get index must be i64");
                    }
                    if type_hash != &type_hash_for("U8") {
                        bail!("lowered string_get result must be u8");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::BoundsCheck {
                    id,
                    index,
                    len: _,
                    len_value,
                    type_hash,
                } => {
                    if value_type(values, index)? != &type_hash_for("I64") {
                        bail!("lowered bounds_check index must be i64");
                    }
                    if let Some(len_value) = len_value
                        && value_type(values, len_value)? != &type_hash_for("I64")
                    {
                        bail!("lowered bounds_check len_value must be i64");
                    }
                    if type_hash != &type_hash_for("Unit") {
                        bail!("lowered bounds_check type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::SliceRangeCheck {
                    id,
                    start,
                    len,
                    source_len,
                    type_hash,
                } => {
                    if value_type(values, start)? != &type_hash_for("I64")
                        || value_type(values, len)? != &type_hash_for("I64")
                        || value_type(values, source_len)? != &type_hash_for("I64")
                    {
                        bail!("lowered slice_range_check values must be i64");
                    }
                    if type_hash != &type_hash_for("Unit") {
                        bail!("lowered slice_range_check type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::LoadEnumTag {
                    id,
                    address,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered load_enum_tag address type mismatch");
                    }
                    if !matches!(self.type_spec_in_root(root, type_hash)?, TypeSpec::Enum(_)) {
                        bail!("lowered load_enum_tag requires enum type");
                    }
                    insert_value(values, id, &type_hash_for("I64"))?;
                }
                LoweredOp::StoreEnumTag {
                    address,
                    type_hash,
                    variant,
                    variant_symbol,
                    tag_value,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered store_enum_tag address type mismatch");
                    }
                    let variant_info =
                        self.lowered_enum_variant(root, target_triple, type_hash, variant)?;
                    if variant_info.variant_symbol != *variant_symbol
                        || variant_info.tag_value != *tag_value
                    {
                        bail!("lowered store_enum_tag metadata mismatch");
                    }
                }
                LoweredOp::Load {
                    id,
                    address,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered load type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                    // A `box<T>` loaded out of a tracked slot/field place carries
                    // that place forward, so a following `DerefBox` can address the
                    // pointee as a tracked `Deref` sub-place (field-granular move/
                    // drop through a box deref; SPEC_V3 §7). Harmless when the loaded
                    // box is never deref'd — the entry is only consumed by `DerefBox`.
                    if self.type_is_box(root, type_hash)?
                        && let Some(place) = drop_state.addr_places.get(address).cloned()
                    {
                        drop_state.loaded_box_place.insert(id.clone(), place);
                    }
                }
                LoweredOp::Store {
                    address,
                    value,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered store address type mismatch");
                    }
                    let value_ty = value_type(values, value)?;
                    if !self.type_assignable_in_root(root, value_ty, type_hash)? {
                        bail!("lowered store value type mismatch");
                    }
                    // Layout backstop for the field/variant-order miscompile
                    // class: a record/enum/array value whose static type is only
                    // *name*-assignable to the destination (e.g. a structural,
                    // alphabetically-ordered literal type vs a named type with a
                    // different declared field/variant order) would blind-copy
                    // its bytes to the wrong offsets. Require byte-layout
                    // compatibility, mirroring `lower_expr_as`, so any value-flow
                    // sink that forgets to build into the destination layout
                    // fails closed here instead of miscompiling. Scalars and
                    // references are layout-trivial under the assignable check.
                    if value_ty != type_hash
                        && (self.type_is_record(root, value_ty)?
                            || self.type_is_record(root, type_hash)?
                            || self.type_is_enum(root, value_ty)?
                            || self.type_is_enum(root, type_hash)?
                            || self.type_is_fixed_array(root, value_ty)?
                            || self.type_is_fixed_array(root, type_hash)?
                            || self.type_is_box(root, value_ty)?
                            || self.type_is_box(root, type_hash)?)
                        && !self.layouts_blind_copy_compatible(
                            root,
                            target_triple,
                            value_ty,
                            type_hash,
                        )?
                    {
                        bail!(
                            "lowered store reinterprets an aggregate value of type {value_ty} under incompatible destination layout {type_hash}"
                        );
                    }
                    if let Some(place) = drop_state.addr_places.get(address).cloned() {
                        // Storing into a place re-initializes it: clear any move
                        // or drop recorded for it or its sub-/super-places.
                        drop_state
                            .dropped
                            .retain(|m| !places_overlap_lowered(&place, m));
                        drop_state.moved.retain(|m| !places_overlap_lowered(&place, m));
                    }
                }
                LoweredOp::Copy {
                    id,
                    value,
                    type_hash,
                } => {
                    if self.type_is_move_only(root, target_triple, type_hash)? {
                        bail!("lowered copy requires a copy type");
                    }
                    if value_type(values, value)? != type_hash {
                        bail!("lowered copy type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                    if self.type_passes_indirect(root, target_triple, type_hash)? {
                        insert_address(addresses, id, type_hash)?;
                    }
                }
                LoweredOp::Move {
                    id,
                    address,
                    type_hash,
                } => {
                    if !self.type_is_move_only(root, target_triple, type_hash)? {
                        bail!("lowered move requires a move-only type");
                    }
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered move type mismatch");
                    }
                    // A move consumes a tracked place: a whole slot, a record-field
                    // / constant-array-index projection, or a field/element reached
                    // through a `box` deref (all field-granular, SPEC_V3 §7). An
                    // untracked address (dynamic array index, raw-pointer deref) is
                    // not granular-droppable, so a partial move through one stays
                    // fail-closed. This is the independent backstop for the lowering
                    // guard in `mark_moved_source`.
                    let Some(place) = drop_state.addr_places.get(address).cloned() else {
                        bail!(
                            "lowered move of a non-whole-slot place (partial move of an owned aggregate)"
                        );
                    };
                    if place_conflicts(&drop_state.moved, &place) {
                        bail!("lowered move of already-moved storage");
                    }
                    if place_conflicts(&drop_state.dropped, &place) {
                        bail!("lowered move of dropped storage");
                    }
                    insert_moved_place(&mut drop_state.moved, place);
                    insert_value(values, id, type_hash)?;
                    if self.type_passes_indirect(root, target_triple, type_hash)? {
                        insert_address(addresses, id, type_hash)?;
                    }
                }
                LoweredOp::Drop { address, type_hash } => {
                    if !self.type_requires_drop_scaffold(root, target_triple, type_hash)? {
                        bail!("lowered drop requires a drop-relevant type");
                    }
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered drop type mismatch");
                    }
                    // SPEC_V2 §20 / SPEC_V3 §7: an owned place is dropped AT MOST
                    // once and never after being moved out — the no-double-free half
                    // of exactly-once. The no-leak half (that a live owned place IS
                    // dropped) is ensured by lowering's static drop placement, not
                    // independently re-proven here. Field-granular: drops of disjoint
                    // fields (`x.a` then `x.b`) are fine; an overlapping move or prior
                    // drop is a double-free.
                    if let Some(place) = drop_state.addr_places.get(address).cloned() {
                        if place_conflicts(&drop_state.moved, &place) {
                            bail!("lowered drop of moved-out storage");
                        }
                        if place_conflicts(&drop_state.dropped, &place) {
                            bail!("lowered double drop of storage");
                        }
                        insert_moved_place(&mut drop_state.dropped, place);
                    }
                    // An enum-payload drop (a consumed move-only enum's payload, e.g.
                    // a `_`/default arm freeing a `box` variant) is not a storage-slot
                    // place; track it by (base, variant) so a repeat is caught as a
                    // double free (SPEC_V3 §7; at-most-once, as above).
                    if let Some(payload) = drop_state.enum_payload_addr.get(address).cloned()
                        && !drop_state.dropped_enum_payloads.insert(payload)
                    {
                        bail!("lowered double drop of enum payload");
                    }
                }
                LoweredOp::FreeBoxShell {
                    address,
                    box_type_hash,
                } => {
                    // Frees a box's heap shell when its pointee was partially moved
                    // (field-granular drop glue through a box deref; SPEC_V3 §7). The
                    // address must name the box slot itself.
                    if address_type(addresses, address)? != box_type_hash {
                        bail!("lowered free_box_shell type mismatch");
                    }
                    if !self.type_is_box(root, box_type_hash)? {
                        bail!("lowered free_box_shell requires a box type");
                    }
                    if let Some(place) = drop_state.addr_places.get(address).cloned() {
                        // At-most-once for the allocation. Each box place names a
                        // distinct heap block (a `box` is a unique owner; a nested
                        // box `h.b` and its container `h` are SEPARATE mallocs), so
                        // double-free is an EXACT-place repeat — not an overlap, which
                        // would wrongly flag freeing both `h` and its inner box `h.b`.
                        if !drop_state.freed_shells.insert(place.clone()) {
                            bail!("lowered double free of box shell");
                        }
                        // A shell whose box (or a containing aggregate) was moved out
                        // wholesale is no longer ours to free — ancestor-or-equal, NOT
                        // overlap: a moved pointee SUB-place (`h.inner`) is exactly the
                        // partial move that motivates this granular shell free.
                        if place_covered_by(&drop_state.moved, &place) {
                            bail!("lowered free of moved-out box shell");
                        }
                    }
                }
                LoweredOp::BorrowDebug {
                    address,
                    mutable: _,
                    region,
                    type_hash,
                } => {
                    if let Some(region) = region
                        && !is_hash(region)
                    {
                        bail!("lowered borrow_debug region is not a hash");
                    }
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered borrow_debug type mismatch");
                    }
                }
                LoweredOp::Return { .. } => {
                    bail!("lowered return is only valid as the final function operation");
                }
                LoweredOp::EarlyReturn { value, type_hash } => {
                    // Early exit (R7): the value placed in the return position must
                    // be a known value of the function's return type. This is the
                    // verifier-side operand-type gate; the early-exit drops were
                    // checked as ordinary `Drop`/`FreeBoxShell` ops preceding it.
                    // `EarlyReturn` terminates its (divergent) block, so nothing
                    // follows it there.
                    if type_hash != return_type {
                        bail!("lowered early return type does not match function return type");
                    }
                    let actual = values.get(value).ok_or_else(|| {
                        anyhow!("lowered early return references unknown value {value}")
                    })?;
                    if !self.type_assignable_in_root(root, actual, type_hash)? {
                        bail!("lowered early return value type mismatch");
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn verify_lowered_block(
        &self,
        root: &ProgramRootPayload,
        block: &LoweredBlock,
        target_triple: &str,
        param_types: &[String],
        return_type: &str,
        local_slots: &[LoweredLocalSlot],
        parent_values: &BTreeMap<String, String>,
        parent_addresses: &BTreeMap<String, String>,
        drop_state: &mut DropTracker,
    ) -> Result<String> {
        let mut values = parent_values.clone();
        let mut addresses = parent_addresses.clone();
        // Values/addresses are branch-local (cloned), but `drop_state` is shared
        // on purpose: local slots and address ids are globally unique within a
        // function and parameters are dropped only at function end, so a move in
        // either branch must remain visible to the function-end drop scaffold.
        self.verify_value_ops(
            root,
            &block.operations,
            target_triple,
            param_types,
            return_type,
            local_slots,
            &mut values,
            &mut addresses,
            drop_state,
        )?;
        value_type(&values, &block.result).cloned()
    }
}

pub(crate) fn lowered_ir_from_artifact_metadata(
    artifact_json: &JsonValue,
) -> Result<LoweredFunctionIr> {
    if artifact_json.get("schema").and_then(JsonValue::as_str) == Some(LOWERED_IR_SCHEMA) {
        return Ok(serde_json::from_value(artifact_json.clone())?);
    }
    let metadata = artifact_json
        .get("metadata")
        .ok_or_else(|| anyhow!("lowered IR artifact missing metadata"))?;
    Ok(serde_json::from_value(metadata.clone())?)
}

fn lowered_ir_layout_target(target_triple: &str) -> &str {
    if target_triple == LOWERING_TARGET {
        DEFAULT_NATIVE_TARGET
    } else {
        target_triple
    }
}

fn hash_lowered_ir_json(value: &JsonValue) -> String {
    hash_bytes(BYTES_DOMAIN, canonical_json(value).as_bytes())
}

fn expr_type(payload: &JsonValue, expr_hash: &str) -> Result<String> {
    payload
        .get("type")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("expression missing type {expr_hash}"))
}

// Built-in operator semantics (source-op -> lowered kind, the inverse verify
// mapping, and trap derivation) live in `crate::op_registry`, the single source
// of truth shared with the reference evaluator and the parser. These remain thin
// forwarders so the lowering call sites and `LoweredTrap` plumbing are untouched.
fn lower_binary_kind(
    source_op: &str,
    left_type: &str,
    right_type: &str,
    result_type: &str,
) -> Result<String> {
    crate::op_registry::lower_binary_kind(source_op, left_type, right_type, result_type)
}

fn trap_for_binary(kind: &str) -> Option<LoweredTrap> {
    crate::op_registry::binary_trap(kind).map(|trap| LoweredTrap {
        condition: trap.condition.to_string(),
        code: trap.code.to_string(),
    })
}

fn verify_binary_kind(
    kind: &str,
    left_type: &str,
    right_type: &str,
    result_type: &str,
    trap: Option<&LoweredTrap>,
) -> Result<()> {
    crate::op_registry::verify_binary_kind(
        kind,
        left_type,
        right_type,
        result_type,
        trap.map(|trap| (trap.condition.as_str(), trap.code.as_str())),
    )
}

fn lower_unary_kind(source_op: &str, input_type: &str, result_type: &str) -> Result<String> {
    crate::op_registry::lower_unary_kind(source_op, input_type, result_type)
}

fn verify_unary_kind(kind: &str, input_type: &str, result_type: &str) -> Result<()> {
    crate::op_registry::verify_unary_kind(kind, input_type, result_type)
}

fn insert_value(values: &mut BTreeMap<String, String>, id: &str, type_hash: &str) -> Result<()> {
    if values
        .insert(id.to_string(), type_hash.to_string())
        .is_some()
    {
        bail!("duplicate lowered value id {id}");
    }
    Ok(())
}

fn insert_address(
    addresses: &mut BTreeMap<String, String>,
    id: &str,
    type_hash: &str,
) -> Result<()> {
    if addresses
        .insert(id.to_string(), type_hash.to_string())
        .is_some()
    {
        bail!("duplicate lowered address id {id}");
    }
    Ok(())
}

fn value_type<'a>(values: &'a BTreeMap<String, String>, id: &str) -> Result<&'a String> {
    values
        .get(id)
        .ok_or_else(|| anyhow!("unknown lowered value id {id}"))
}

fn address_type<'a>(addresses: &'a BTreeMap<String, String>, id: &str) -> Result<&'a String> {
    addresses
        .get(id)
        .ok_or_else(|| anyhow!("unknown lowered address id {id}"))
}

pub(crate) fn lowered_op_id_for_value(value_id: &str) -> String {
    format!("op:{value_id}")
}

pub(crate) fn lowered_op_value_id(op: &LoweredOp) -> Option<&str> {
    match op {
        LoweredOp::Param { id, .. }
        | LoweredOp::ConstI64 { id, .. }
        | LoweredOp::ConstBool { id, .. }
        | LoweredOp::ConstUnit { id, .. }
        | LoweredOp::Unary { id, .. }
        | LoweredOp::IntCast { id, .. }
        | LoweredOp::Binary { id, .. }
        | LoweredOp::Call { id, .. }
        | LoweredOp::If { id, .. }
        | LoweredOp::Case { id, .. }
        | LoweredOp::Fold { id, .. }
        | LoweredOp::Loop { id, .. }
        | LoweredOp::BorrowShared { id, .. }
        | LoweredOp::BorrowMut { id, .. }
        | LoweredOp::DerefShared { id, .. }
        | LoweredOp::DerefMut { id, .. }
        | LoweredOp::DerefBox { id, .. }
        | LoweredOp::UnboxMove { id, .. }
        | LoweredOp::HeapAlloc { id, .. }
        | LoweredOp::PtrCast { id, .. }
        | LoweredOp::DerefRaw { id, .. }
        | LoweredOp::AddrOfParam { id, .. }
        | LoweredOp::AddrOfLocal { id, .. }
        | LoweredOp::AddrOfField { id, .. }
        | LoweredOp::AddrOfEnumPayload { id, .. }
        | LoweredOp::AddrOfIndex { id, .. }
        | LoweredOp::StaticDataAddress { id, .. }
        | LoweredOp::ConstructSlice { id, .. }
        | LoweredOp::SliceLen { id, .. }
        | LoweredOp::SliceData { id, .. }
        | LoweredOp::VecNew { id, .. }
        | LoweredOp::VecPush { id, .. }
        | LoweredOp::VecGet { id, .. }
        | LoweredOp::VecLen { id, .. }
        | LoweredOp::StringNew { id, .. }
        | LoweredOp::StringLen { id, .. }
        | LoweredOp::StringWithCapacity { id, .. }
        | LoweredOp::StringPush { id, .. }
        | LoweredOp::StringGet { id, .. }
        | LoweredOp::BoundsCheck { id, .. }
        | LoweredOp::SliceRangeCheck { id, .. }
        | LoweredOp::LoadEnumTag { id, .. }
        | LoweredOp::Load { id, .. }
        | LoweredOp::Copy { id, .. }
        | LoweredOp::Move { id, .. } => Some(id),
        LoweredOp::Store { .. }
        | LoweredOp::StoreEnumTag { .. }
        | LoweredOp::Drop { .. }
        | LoweredOp::FreeBoxShell { .. }
        | LoweredOp::BorrowDebug { .. }
        | LoweredOp::Return { .. }
        // `EarlyReturn` places an existing value (R7); it defines no new value id.
        | LoweredOp::EarlyReturn { .. } => None,
    }
}

pub(crate) fn lowered_op_kind_name(op: &LoweredOp) -> &'static str {
    match op {
        LoweredOp::Param { .. } => "param",
        LoweredOp::ConstI64 { .. } => "const_i64",
        LoweredOp::ConstBool { .. } => "const_bool",
        LoweredOp::ConstUnit { .. } => "const_unit",
        LoweredOp::Unary { .. } => "unary",
        LoweredOp::IntCast { .. } => "int_cast",
        LoweredOp::Binary { .. } => "binary",
        LoweredOp::Call { .. } => "call",
        LoweredOp::If { .. } => "if",
        LoweredOp::Case { .. } => "case",
        LoweredOp::Fold { .. } => "fold",
        LoweredOp::Loop { .. } => "loop",
        LoweredOp::EarlyReturn { .. } => "early_return",
        LoweredOp::BorrowShared { .. } => "borrow_shared",
        LoweredOp::BorrowMut { .. } => "borrow_mut",
        LoweredOp::DerefShared { .. } => "deref_shared",
        LoweredOp::DerefMut { .. } => "deref_mut",
        LoweredOp::DerefBox { .. } => "deref_box",
        LoweredOp::UnboxMove { .. } => "unbox_move",
        LoweredOp::HeapAlloc { .. } => "heap_alloc",
        LoweredOp::PtrCast { .. } => "ptr_cast",
        LoweredOp::DerefRaw { .. } => "deref_raw",
        LoweredOp::AddrOfParam { .. } => "addr_of_param",
        LoweredOp::AddrOfLocal { .. } => "addr_of_local",
        LoweredOp::AddrOfField { .. } => "addr_of_field",
        LoweredOp::AddrOfEnumPayload { .. } => "addr_of_enum_payload",
        LoweredOp::AddrOfIndex { .. } => "addr_of_index",
        LoweredOp::StaticDataAddress { .. } => "static_data_address",
        LoweredOp::ConstructSlice { .. } => "construct_slice",
        LoweredOp::SliceLen { .. } => "slice_len",
        LoweredOp::SliceData { .. } => "slice_data",
        LoweredOp::VecNew { .. } => "vec_new",
        LoweredOp::VecPush { .. } => "vec_push",
        LoweredOp::VecGet { .. } => "vec_get",
        LoweredOp::VecLen { .. } => "vec_len",
        LoweredOp::StringNew { .. } => "string_new",
        LoweredOp::StringLen { .. } => "string_len",
        LoweredOp::StringWithCapacity { .. } => "string_with_capacity",
        LoweredOp::StringPush { .. } => "string_push",
        LoweredOp::StringGet { .. } => "string_get",
        LoweredOp::BoundsCheck { .. } => "bounds_check",
        LoweredOp::SliceRangeCheck { .. } => "slice_range_check",
        LoweredOp::LoadEnumTag { .. } => "load_enum_tag",
        LoweredOp::StoreEnumTag { .. } => "store_enum_tag",
        LoweredOp::Load { .. } => "load",
        LoweredOp::Store { .. } => "store",
        LoweredOp::Copy { .. } => "copy",
        LoweredOp::Move { .. } => "move",
        LoweredOp::Drop { .. } => "drop",
        LoweredOp::FreeBoxShell { .. } => "free_box_shell",
        LoweredOp::BorrowDebug { .. } => "borrow_debug",
        LoweredOp::Return { .. } => "return",
    }
}

fn default_slot_size_bytes() -> u64 {
    8
}

fn is_default_slot_size(value: &u64) -> bool {
    *value == default_slot_size_bytes()
}

fn stack_slot_size_bytes(layout_size_bytes: u64) -> u64 {
    layout_size_bytes.max(1).div_ceil(8) * 8
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn lowered_value_debug_ops(
    ir: &LoweredFunctionIr,
) -> Result<BTreeMap<String, LoweredDebugOp>> {
    let expected_values = lowered_value_debug_infos(&ir.operations)?
        .into_keys()
        .collect::<BTreeSet<_>>();
    let mut out = BTreeMap::new();
    for op in &ir.debug_map.operations {
        if expected_values.contains(&op.value_id) {
            out.insert(op.value_id.clone(), op.clone());
        }
    }
    Ok(out)
}

fn lowered_value_debug_infos(operations: &[LoweredOp]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    collect_lowered_value_debug_infos(operations, &mut out)?;
    Ok(out)
}

fn collect_lowered_value_debug_infos(
    operations: &[LoweredOp],
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    for op in operations {
        if let Some(value_id) = lowered_op_value_id(op)
            && out
                .insert(value_id.to_string(), lowered_op_kind_name(op).to_string())
                .is_some()
        {
            bail!("duplicate lowered value id {value_id}");
        }
        if let LoweredOp::If {
            then_block,
            else_block,
            ..
        } = op
        {
            collect_lowered_value_debug_infos(&then_block.operations, out)?;
            collect_lowered_value_debug_infos(&else_block.operations, out)?;
        } else {
            match op {
                LoweredOp::Case { arms, .. } => {
                    for arm in arms {
                        collect_lowered_value_debug_infos(&arm.block.operations, out)?;
                    }
                }
                LoweredOp::Fold { body, .. } => {
                    collect_lowered_value_debug_infos(&body.operations, out)?;
                }
                LoweredOp::Loop { cond, body, .. } => {
                    collect_lowered_value_debug_infos(&cond.operations, out)?;
                    collect_lowered_value_debug_infos(&body.operations, out)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn collect_op_type_hashes(operations: &[LoweredOp], out: &mut BTreeSet<String>) {
    for op in operations {
        match op {
            LoweredOp::Param { type_hash, .. }
            | LoweredOp::ConstI64 { type_hash, .. }
            | LoweredOp::ConstBool { type_hash, .. }
            | LoweredOp::ConstUnit { type_hash, .. }
            | LoweredOp::Unary { type_hash, .. }
            | LoweredOp::IntCast { type_hash, .. }
            | LoweredOp::Binary { type_hash, .. }
            | LoweredOp::Call { type_hash, .. }
            | LoweredOp::If { type_hash, .. }
            | LoweredOp::Case { type_hash, .. }
            | LoweredOp::BorrowShared { type_hash, .. }
            | LoweredOp::BorrowMut { type_hash, .. }
            | LoweredOp::BoundsCheck { type_hash, .. }
            | LoweredOp::SliceRangeCheck { type_hash, .. }
            | LoweredOp::LoadEnumTag { type_hash, .. }
            | LoweredOp::Load { type_hash, .. }
            | LoweredOp::Store { type_hash, .. }
            | LoweredOp::StoreEnumTag { type_hash, .. }
            | LoweredOp::Copy { type_hash, .. }
            | LoweredOp::Move { type_hash, .. }
            | LoweredOp::Drop { type_hash, .. }
            | LoweredOp::BorrowDebug { type_hash, .. }
            | LoweredOp::Return { type_hash, .. }
            | LoweredOp::EarlyReturn { type_hash, .. } => {
                out.insert(type_hash.clone());
            }
            LoweredOp::HeapAlloc {
                type_hash,
                element_type_hash,
                ..
            } => {
                out.insert(type_hash.clone());
                out.insert(element_type_hash.clone());
            }
            LoweredOp::DerefBox {
                box_type_hash,
                element_type_hash,
                ..
            }
            | LoweredOp::UnboxMove {
                box_type_hash,
                element_type_hash,
                ..
            } => {
                out.insert(box_type_hash.clone());
                out.insert(element_type_hash.clone());
            }
            LoweredOp::FreeBoxShell { box_type_hash, .. } => {
                out.insert(box_type_hash.clone());
            }
            LoweredOp::PtrCast {
                source_type_hash,
                type_hash,
                ..
            } => {
                out.insert(source_type_hash.clone());
                out.insert(type_hash.clone());
            }
            LoweredOp::DerefRaw {
                pointer_type_hash,
                pointee_type_hash,
                ..
            } => {
                out.insert(pointer_type_hash.clone());
                out.insert(pointee_type_hash.clone());
            }
            LoweredOp::ConstructSlice {
                type_hash,
                element_type_hash,
                ..
            } => {
                out.insert(type_hash.clone());
                out.insert(element_type_hash.clone());
            }
            LoweredOp::StaticDataAddress {
                element_type_hash, ..
            } => {
                out.insert(element_type_hash.clone());
            }
            LoweredOp::SliceLen {
                slice_type_hash, ..
            } => {
                out.insert(slice_type_hash.clone());
            }
            LoweredOp::SliceData {
                slice_type_hash,
                element_type_hash,
                ..
            } => {
                out.insert(slice_type_hash.clone());
                out.insert(element_type_hash.clone());
            }
            LoweredOp::VecNew {
                type_hash,
                element_type_hash,
                ..
            } => {
                out.insert(type_hash.clone());
                out.insert(element_type_hash.clone());
            }
            LoweredOp::VecPush {
                type_hash,
                vec_type_hash,
                element_type_hash,
                ..
            }
            | LoweredOp::VecGet {
                type_hash,
                vec_type_hash,
                element_type_hash,
                ..
            } => {
                out.insert(type_hash.clone());
                out.insert(vec_type_hash.clone());
                out.insert(element_type_hash.clone());
            }
            LoweredOp::VecLen {
                type_hash,
                vec_type_hash,
                ..
            } => {
                out.insert(type_hash.clone());
                out.insert(vec_type_hash.clone());
            }
            LoweredOp::StringNew { type_hash, .. } => {
                out.insert(type_hash.clone());
            }
            LoweredOp::StringLen {
                type_hash,
                string_type_hash,
                ..
            } => {
                out.insert(type_hash.clone());
                out.insert(string_type_hash.clone());
            }
            LoweredOp::StringWithCapacity { type_hash, .. } => {
                out.insert(type_hash.clone());
            }
            LoweredOp::StringPush {
                type_hash,
                string_type_hash,
                ..
            }
            | LoweredOp::StringGet {
                type_hash,
                string_type_hash,
                ..
            } => {
                out.insert(type_hash.clone());
                out.insert(string_type_hash.clone());
            }
            LoweredOp::Fold {
                target_type_hash,
                element_type_hash,
                acc_type_hash,
                body,
                ..
            } => {
                out.insert(target_type_hash.clone());
                out.insert(element_type_hash.clone());
                out.insert(acc_type_hash.clone());
                collect_op_type_hashes(&body.operations, out);
            }
            LoweredOp::Loop {
                acc_type_hash,
                type_hash,
                cond,
                body,
                ..
            } => {
                out.insert(acc_type_hash.clone());
                out.insert(type_hash.clone());
                collect_op_type_hashes(&cond.operations, out);
                collect_op_type_hashes(&body.operations, out);
            }
            LoweredOp::DerefShared {
                referent_type_hash, ..
            }
            | LoweredOp::DerefMut {
                referent_type_hash, ..
            } => {
                out.insert(referent_type_hash.clone());
            }
            LoweredOp::AddrOfParam { place, .. }
            | LoweredOp::AddrOfLocal { place, .. }
            | LoweredOp::AddrOfField { place, .. }
            | LoweredOp::AddrOfEnumPayload { place, .. }
            | LoweredOp::AddrOfIndex { place, .. } => collect_place_type_hashes(place, out),
        }
        if let LoweredOp::If {
            then_block,
            else_block,
            ..
        } = op
        {
            collect_op_type_hashes(&then_block.operations, out);
            collect_op_type_hashes(&else_block.operations, out);
        } else if let LoweredOp::Case {
            enum_type_hash,
            arms,
            ..
        } = op
        {
            out.insert(enum_type_hash.clone());
            for arm in arms {
                out.insert(arm.payload_type_hash.clone());
                collect_op_type_hashes(&arm.block.operations, out);
            }
        }
    }
}

fn collect_place_type_hashes(place: &LoweredPlace, out: &mut BTreeSet<String>) {
    match place {
        LoweredPlace::Param { type_hash, .. } | LoweredPlace::Local { type_hash, .. } => {
            out.insert(type_hash.clone());
        }
        LoweredPlace::Field {
            owner_type_hash,
            type_hash,
            ..
        }
        | LoweredPlace::EnumPayload {
            owner_type_hash,
            type_hash,
            ..
        } => {
            out.insert(owner_type_hash.clone());
            out.insert(type_hash.clone());
        }
        LoweredPlace::Index {
            element_type_hash,
            type_hash,
            ..
        } => {
            out.insert(element_type_hash.clone());
            out.insert(type_hash.clone());
        }
    }
}

fn required_layout_string(metadata: &JsonValue, key: &str) -> Result<String> {
    metadata
        .get(key)
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("type layout missing string {key}"))
}

fn required_layout_u64(metadata: &JsonValue, key: &str) -> Result<u64> {
    metadata
        .get(key)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| anyhow!("type layout missing integer {key}"))
}

struct EnumVariantLayout {
    variant_symbol: Option<String>,
    type_hash: String,
    tag_value: u64,
    payload_offset_bytes: u64,
}

fn enum_variant_layout(layout: &JsonValue, variant: &str) -> Result<EnumVariantLayout> {
    if layout.get("kind").and_then(JsonValue::as_str) != Some("enum") {
        bail!("type layout metadata must have kind enum");
    }
    let entry = layout
        .get("variants")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .find(|entry| entry.get("name").and_then(JsonValue::as_str) == Some(variant))
        .ok_or_else(|| anyhow!("enum layout missing variant {variant}"))?;
    Ok(EnumVariantLayout {
        variant_symbol: entry
            .get("variant_symbol")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        type_hash: entry
            .get("type_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("enum layout variant {variant} missing type_hash"))?
            .to_string(),
        tag_value: entry
            .get("tag_value")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("enum layout variant {variant} missing tag_value"))?,
        payload_offset_bytes: entry
            .get("payload_offset_bytes")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("enum layout variant {variant} missing payload_offset_bytes"))?,
    })
}

fn is_hash(value: &str) -> bool {
    value.starts_with("sha256:")
}

fn local_lowered_at_depth(
    locals: &[LocalLoweredBinding],
    depth: usize,
) -> Option<&LocalLoweredBinding> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
}
