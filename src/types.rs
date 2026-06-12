use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::backend::ArtifactKind;
use crate::expr::{RawCaseArm, RawExpr, RawPattern};
use crate::model::{
    ProgramRootPayload, TypeCheckResult, resolve_function_name_in_root, resolve_named_type_in_root,
    validate_projection_identifier,
};
use crate::store::{CodeDb, canonical_json, hash_object_canonical};
use crate::{ABI_TAG, DEFAULT_NATIVE_TARGET, MAIN_BRANCH, SCHEMA_VERSION};

pub(crate) fn static_region_hash() -> String {
    hash_object_canonical(
        "SymbolBirth",
        SCHEMA_VERSION,
        &canonical_json(&static_region_payload()),
    )
}

pub(crate) fn is_static_region(region: &str) -> bool {
    region == static_region_hash()
}

fn static_region_payload() -> JsonValue {
    json!({
        "symbol_kind": "region_param",
        "birth_history_hash": "genesis",
        "local_nonce": "builtin:static",
    })
}

pub(crate) fn static_data_payload(bytes_hex: &str, len: usize) -> Result<JsonValue> {
    validate_hex_bytes(bytes_hex)?;
    if bytes_hex.len() != len * 2 {
        bail!("static data len does not match bytes_hex");
    }
    Ok(json!({
        "schema": "codedb/static-data/v1",
        "bytes_hex": bytes_hex,
        "len": len as u64,
    }))
}

pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    validate_hex_bytes(hex)?;
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let raw = hex.as_bytes();
    for idx in (0..raw.len()).step_by(2) {
        bytes.push((hex_value(raw[idx])? << 4) | hex_value(raw[idx + 1])?);
    }
    Ok(bytes)
}

pub(crate) fn validate_hex_bytes(hex: &str) -> Result<()> {
    if !hex.len().is_multiple_of(2) {
        bail!("hex byte string must have even length");
    }
    for byte in hex.bytes() {
        let valid = byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
        if !valid {
            bail!("hex byte string must use lowercase hex digits");
        }
    }
    Ok(())
}

fn hex_value(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => bail!("invalid hex digit"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeMemberSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SymbolBirthSpec {
    pub(crate) symbol_kind: String,
    pub(crate) birth_history_hash: String,
    pub(crate) local_nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner_type_symbol: Option<String>,
}

impl SymbolBirthSpec {
    pub(crate) fn from_payload(payload: &JsonValue) -> Result<Self> {
        let symbol_kind = payload
            .get("symbol_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("SymbolBirth missing symbol_kind"))?
            .to_string();
        let birth_history_hash = payload
            .get("birth_history_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("SymbolBirth missing birth_history_hash"))?
            .to_string();
        let local_nonce = payload
            .get("local_nonce")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("SymbolBirth missing local_nonce"))?
            .to_string();
        let owner_type_symbol = payload
            .get("owner_type_symbol")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        Ok(Self {
            symbol_kind,
            birth_history_hash,
            local_nonce,
            owner_type_symbol,
        })
    }

    pub(crate) fn to_payload(&self) -> JsonValue {
        let mut payload = json!({
            "symbol_kind": self.symbol_kind.clone(),
            "birth_history_hash": self.birth_history_hash.clone(),
            "local_nonce": self.local_nonce.clone(),
        });
        if let Some(owner_type_symbol) = &self.owner_type_symbol {
            payload["owner_type_symbol"] = JsonValue::String(owner_type_symbol.clone());
        }
        payload
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeDefinitionIdentity {
    pub(crate) type_symbol_birth: SymbolBirthSpec,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) region_param_births: Vec<SymbolBirthSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) member_births: Vec<SymbolBirthSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TypeDefinitionKind {
    Record { fields: Vec<TypeMemberSpec> },
    Enum { variants: Vec<TypeMemberSpec> },
}

impl TypeDefinitionKind {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            TypeDefinitionKind::Record { .. } => "record",
            TypeDefinitionKind::Enum { .. } => "enum",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegionParamDef {
    pub(crate) region: String,
    pub(crate) name: String,
}

/// A type parameter on a generic record/enum/function definition (R11). Its
/// identity is positional (the index of the slot it occupies), so only the
/// display `name` is carried — no birth symbol — and a rename of the parameter
/// is a pure projection change that does not re-identify the generic. A use of
/// the parameter inside the body resolves to `TypeSpec::TypeParam { index }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeParamDef {
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeMemberDef {
    pub(crate) member_symbol: String,
    pub(crate) name: String,
    pub(crate) type_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypeDefinition {
    Record {
        type_symbol: String,
        region_params: Vec<RegionParamDef>,
        type_params: Vec<TypeParamDef>,
        fields: Vec<TypeMemberDef>,
    },
    Enum {
        type_symbol: String,
        region_params: Vec<RegionParamDef>,
        type_params: Vec<TypeParamDef>,
        variants: Vec<TypeMemberDef>,
    },
}

impl TypeDefinition {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            TypeDefinition::Record { .. } => "record",
            TypeDefinition::Enum { .. } => "enum",
        }
    }

    pub(crate) fn type_symbol(&self) -> &str {
        match self {
            TypeDefinition::Record { type_symbol, .. }
            | TypeDefinition::Enum { type_symbol, .. } => type_symbol,
        }
    }

    pub(crate) fn region_params(&self) -> &[RegionParamDef] {
        match self {
            TypeDefinition::Record { region_params, .. }
            | TypeDefinition::Enum { region_params, .. } => region_params,
        }
    }

    pub(crate) fn type_params(&self) -> &[TypeParamDef] {
        match self {
            TypeDefinition::Record { type_params, .. }
            | TypeDefinition::Enum { type_params, .. } => type_params,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Pure,
    Trap,
    Io,
    State,
    Alloc,
    Ffi,
    Unsafe,
    Concurrent,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Pure => "pure",
            Effect::Trap => "trap",
            Effect::Io => "io",
            Effect::State => "state",
            Effect::Alloc => "alloc",
            Effect::Ffi => "ffi",
            Effect::Unsafe => "unsafe",
            Effect::Concurrent => "concurrent",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self> {
        match value {
            "pure" => Ok(Effect::Pure),
            "trap" => Ok(Effect::Trap),
            "io" => Ok(Effect::Io),
            "state" => Ok(Effect::State),
            "alloc" => Ok(Effect::Alloc),
            "ffi" => Ok(Effect::Ffi),
            "unsafe" => Ok(Effect::Unsafe),
            "concurrent" => Ok(Effect::Concurrent),
            other => bail!("unknown effect {other}"),
        }
    }
}

#[derive(Debug, Clone)]
struct LocalTypeBinding {
    name: String,
    type_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoanKind {
    Shared,
    Mutable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoanRoot {
    Param(usize),
    Local(usize),
    Static(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoanPlace {
    root: LoanRoot,
    fields: Vec<String>,
}

impl LoanPlace {
    fn with_field(&self, field: &str) -> Self {
        let mut place = self.clone();
        place.fields.push(field.to_string());
        place
    }

    fn with_segment(&self, segment: String) -> Self {
        let mut place = self.clone();
        place.fields.push(segment);
        place
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveLoan {
    kind: LoanKind,
    region: String,
    place: LoanPlace,
    owner: Option<LoanPlace>,
    // Reference- and slice-derived loans are `exclusive` and participate in
    // aliasing checks (a mutable loan excludes other loans / reads / moves of
    // the same place). Raw-pointer-derived loans are NOT exclusive: SPEC §15
    // says raw pointers carry no region guarantees and dereferencing one is the
    // caller's `unsafe` responsibility, so they may legally alias. They are
    // still tracked (for liveness/escape: a raw pointer must not outlive the
    // storage it points into), but are skipped by the exclusivity checks.
    exclusive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueCopyKind {
    Copy,
    MoveOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueDropKind {
    Trivial,
    NeedsDrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValueClass {
    copy_kind: ValueCopyKind,
    drop_kind: ValueDropKind,
    contains_reference: bool,
    contains_mut_reference: bool,
    contains_box: bool,
}

/// The move constraint active inside a multi-execution region (a `loop`
/// cond/body or a `fold` body), which runs 0..N times per surrounding
/// evaluation (#14a → loop-carried drop glue, SPEC_V3 §7). Storage that
/// OUTLIVES one iteration (a param, a static, or a local bound outside the
/// region) must not be moved — the move would repeat. Storage minted inside
/// the region (local ids at or above `floor`) dies with the iteration and may
/// move freely; its scoped drop glue re-executes per iteration. A `loop`'s
/// accumulator (`movable_whole`) is the one outer place a BODY may consume:
/// the back-edge refills it (the body result stores over it), with lowering
/// dropping the old value when the body did NOT consume it.
#[derive(Debug, Clone, Copy)]
struct IterationMoveScope {
    /// Local ids at or above this are per-iteration storage.
    floor: usize,
    /// The loop accumulator's local id: movable, but only as a whole place
    /// (partial accumulator moves stay fail-closed).
    movable_whole: Option<usize>,
    /// "loop body" / "loop condition" / "fold body", for diagnostics.
    construct: &'static str,
}

#[derive(Debug, Clone)]
struct MoveBorrowState {
    locals: Vec<usize>,
    active: Vec<ActiveLoan>,
    moved: Vec<LoanPlace>,
    next_local: usize,
    /// The innermost multi-execution region's move constraint, when inside
    /// one. Checked at the move-recording site, so scope pops cannot hide a
    /// per-iteration move the way the retired `moved` set could.
    iteration_scope: Option<IterationMoveScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExprUse {
    /// The expression's value is read (and, for a move-only type, moved). The
    /// whole place — and every sub-place — must be live.
    Value,
    /// The expression names a whole storage location used as-is (a borrow or
    /// assignment target). The whole place must be live.
    Place,
    /// The expression names a storage location we will immediately narrow into
    /// (the base of a `field_access`/`array_index`). With field-granular drop
    /// glue (SPEC_V3 §7) a *sibling* sub-place may have been moved out, so only
    /// a move of this place itself or an ancestor invalidates the projection;
    /// the narrowed leaf place is checked separately at full granularity.
    ProjectionBase,
}

/// The scalar type a literal `case` (R14) dispatches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarCaseKind {
    I64,
    Bool,
}

impl std::fmt::Display for ScalarCaseKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScalarCaseKind::I64 => write!(f, "i64"),
            ScalarCaseKind::Bool => write!(f, "bool"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IndexedElementInfo {
    FixedArray {
        container_type: String,
        element_type: String,
        len: u64,
    },
    Slice {
        container_type: String,
        element_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeFieldSpec {
    pub(crate) name: String,
    pub(crate) type_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypeSpec {
    Builtin(String),
    Named {
        type_symbol: String,
        region_args: Vec<String>,
        /// Type arguments instantiating a generic record/enum (R11). Empty for a
        /// non-generic named type, so its Type-object payload — and therefore its
        /// content hash — is byte-identical to the pre-generics form. A non-empty
        /// list makes this Named a *generic instance* whose content hash (derived
        /// from the generic's `type_symbol` plus the argument hashes) is the
        /// instance's stable identity; the concrete structure is materialized on
        /// demand by substituting the arguments into the generic's template
        /// (`type_spec_in_root`), i.e. monomorphized at use/layout/lowering, never
        /// stored as a separate object.
        type_args: Vec<String>,
    },
    /// A reference to the enclosing generic definition's type parameter, by
    /// positional index (R11). Like a `param_ref` for types: the parameter
    /// *names* live on the generic `TypeDefinition`/signature, so the hash is
    /// name-independent (two generics with differently-named but structurally
    /// identical parameters instantiate to identical hashes). Only ever appears
    /// inside a generic template (a field/variant/param/return type); it is the
    /// opaque type during generic-body type checking (it has no fields and no
    /// arithmetic, which is exactly constraint-free parametricity) and is
    /// substituted away before any concrete layout/lowering.
    TypeParam {
        index: u32,
    },
    Reference {
        region: String,
        mutable: bool,
        referent: String,
    },
    RawPointer {
        mutable: bool,
        pointee: String,
    },
    Box {
        element: String,
    },
    Vec {
        element: String,
    },
    String,
    Slice {
        region: String,
        mutable: bool,
        element: String,
    },
    FixedArray {
        element: String,
        len: u64,
    },
    Record(Vec<TypeFieldSpec>),
    Enum(Vec<TypeFieldSpec>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalFunctionMetadata {
    pub(crate) symbol: String,
    pub(crate) signature: String,
    pub(crate) abi: String,
    pub(crate) link_name: String,
    pub(crate) library: Option<String>,
}

/// One builtin scalar integer type: its registry name, byte width, and
/// signedness. THE single source of truth for the sized-integer surface (Phase 9,
/// R5) — builtin registration, layout sizing, parser recognition, source
/// projection, the evaluator's `Value` model, native loads/stores, and the
/// operator registry all derive from this list, so a new width is one row here
/// plus the layers it forces. Ordered widening within each signedness (used by
/// cast and the conformance ordering).
pub(crate) struct ScalarIntType {
    pub(crate) name: &'static str,
    pub(crate) width: u64,
    pub(crate) signed: bool,
}

pub(crate) const SCALAR_INT_TYPES: &[ScalarIntType] = &[
    ScalarIntType { name: "I8", width: 1, signed: true },
    ScalarIntType { name: "I16", width: 2, signed: true },
    ScalarIntType { name: "I32", width: 4, signed: true },
    ScalarIntType { name: "I64", width: 8, signed: true },
    ScalarIntType { name: "U8", width: 1, signed: false },
    ScalarIntType { name: "U16", width: 2, signed: false },
    ScalarIntType { name: "U32", width: 4, signed: false },
    ScalarIntType { name: "U64", width: 8, signed: false },
];

/// The `ScalarIntType` for a registry name (`"I32"`, `"U8"`, …), or `None` for a
/// non-integer builtin (`Bool`, `Unit`) or a non-builtin type.
pub(crate) fn scalar_int_type(name: &str) -> Option<&'static ScalarIntType> {
    SCALAR_INT_TYPES.iter().find(|t| t.name == name)
}

/// The lowercase source spelling of a scalar-int registry name (`"I32"` ⇒
/// `"i32"`), used by both the parser (accepting either case) and projection.
pub(crate) fn scalar_int_source_name(name: &str) -> Option<String> {
    scalar_int_type(name).map(|t| t.name.to_ascii_lowercase())
}

/// Resolve a source type identifier (`"u32"`, `"U32"`) to its registry name, for
/// the parser and the type-annotation normalizer.
pub(crate) fn scalar_int_name_for_source(ident: &str) -> Option<&'static str> {
    SCALAR_INT_TYPES
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(ident))
        .map(|t| t.name)
}

/// The lowercase source spelling of a builtin scalar integer type *by content
/// hash* (`type_hash_for("U32")` ⇒ `"u32"`), or `None` if `hash` is not one. The
/// hash→name resolvers (`type_name`, `type_kind_str`, …) route scalar ints here.
pub(crate) fn scalar_int_source_name_for_hash(hash: &str) -> Option<String> {
    SCALAR_INT_TYPES
        .iter()
        .find(|t| hash == type_hash_for(t.name))
        .map(|t| t.name.to_ascii_lowercase())
}

/// The [`ScalarIntType`] a content hash names, or `None` if `hash` is not a
/// scalar integer type. The operator type-checker/verifier route their
/// "is this a sized integer operand" gate through this so the set of integer
/// operand types is exactly `SCALAR_INT_TYPES`.
pub(crate) fn scalar_int_type_by_hash(hash: &str) -> Option<&'static ScalarIntType> {
    SCALAR_INT_TYPES.iter().find(|t| hash == type_hash_for(t.name))
}

/// The target [`ScalarIntType`] of an integer cast builtin (`to_u32`, `to_i8`, …),
/// or `None` if `name` is not one. A cast is `to_<lowercase-width>(value)` — the
/// width in the name keeps casts to a single builtin per target without any new
/// type-argument syntax (R6, Phase 9).
pub(crate) fn int_cast_target(name: &str) -> Option<&'static ScalarIntType> {
    name.strip_prefix("to_").and_then(scalar_int_name_for_source).and_then(scalar_int_type)
}

/// Whether the decimal/`0x`-hex literal text `value` is in range for `int`'s width
/// and signedness. The single check both the type-checker (context-typed literals)
/// and the evaluator route through, so "fits the width" means exactly one thing.
pub(crate) fn int_literal_in_range(value: &str, int: &ScalarIntType) -> bool {
    let (radix, digits) = match value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        Some(hex) => (16, hex),
        None => (10, value),
    };
    // A hex literal at a SIGNED width is a bit pattern (#9): `0x80` as i8 is
    // -128, `0xff` is -1 — parsed at the unsigned width, reinterpreted. The
    // previously accepted range (no high bit) parses to the same values, so
    // only previously-REJECTED literals gain meaning. Decimal stays a signed
    // value (incl. a leading `-`, used by the negated-literal fold).
    let signed_hex = int.signed && radix == 16;
    match (int.signed, int.width) {
        (true, 1) if signed_hex => u8::from_str_radix(digits, radix).is_ok(),
        (true, 2) if signed_hex => u16::from_str_radix(digits, radix).is_ok(),
        (true, 4) if signed_hex => u32::from_str_radix(digits, radix).is_ok(),
        (true, 8) if signed_hex => u64::from_str_radix(digits, radix).is_ok(),
        (true, 1) => i8::from_str_radix(digits, radix).is_ok(),
        (true, 2) => i16::from_str_radix(digits, radix).is_ok(),
        (true, 4) => i32::from_str_radix(digits, radix).is_ok(),
        (true, 8) => i64::from_str_radix(digits, radix).is_ok(),
        (false, 1) => u8::from_str_radix(digits, radix).is_ok(),
        (false, 2) => u16::from_str_radix(digits, radix).is_ok(),
        (false, 4) => u32::from_str_radix(digits, radix).is_ok(),
        (false, 8) => u64::from_str_radix(digits, radix).is_ok(),
        _ => false,
    }
}

impl CodeDb {
    pub(crate) fn insert_builtin_types(&mut self) -> Result<()> {
        for type_name in ["Bool", "Unit"]
            .iter()
            .copied()
            .chain(SCALAR_INT_TYPES.iter().map(|t| t.name))
        {
            self.put_object("Type", &json!({ "type_kind": type_name }))?;
        }
        self.put_object("SymbolBirth", &static_region_payload())?;
        Ok(())
    }

    pub(crate) fn put_type_symbol_birth(
        &mut self,
        parent_history_hash: Option<&str>,
        birth_seed: &str,
    ) -> Result<String> {
        self.put_symbol_birth_with_kind(parent_history_hash, birth_seed, "type")
    }

    pub(crate) fn symbol_birth_spec(&self, symbol: &str) -> Result<SymbolBirthSpec> {
        let kind = self.get_kind(symbol)?;
        if kind != "SymbolBirth" {
            bail!("symbol {symbol} is {kind}, not SymbolBirth");
        }
        SymbolBirthSpec::from_payload(&self.get_payload(symbol)?)
    }

    pub(crate) fn put_symbol_birth_spec(
        &mut self,
        spec: &SymbolBirthSpec,
        expected_kind: &str,
        expected_owner_type_symbol: Option<&str>,
    ) -> Result<String> {
        if spec.symbol_kind != expected_kind {
            bail!(
                "projection identity expected {expected_kind} SymbolBirth, got {}",
                spec.symbol_kind
            );
        }
        match (
            expected_owner_type_symbol,
            spec.owner_type_symbol.as_deref(),
        ) {
            (Some(expected), Some(actual)) if actual == expected => {}
            (Some(expected), Some(actual)) => {
                bail!("projection identity owner mismatch: expected {expected}, got {actual}")
            }
            (Some(expected), None) => {
                bail!("projection identity missing owner type symbol {expected}")
            }
            (None, Some(actual)) => bail!("projection identity has unexpected owner {actual}"),
            (None, None) => {}
        }
        self.put_object("SymbolBirth", &spec.to_payload())
    }

    pub(crate) fn put_region_param_birth(
        &mut self,
        parent_history_hash: Option<&str>,
        owner_type_symbol: &str,
        birth_seed: &str,
    ) -> Result<String> {
        self.put_owned_symbol_birth(
            parent_history_hash,
            birth_seed,
            "region_param",
            owner_type_symbol,
        )
    }

    pub(crate) fn put_record_field_birth(
        &mut self,
        parent_history_hash: Option<&str>,
        owner_type_symbol: &str,
        birth_seed: &str,
    ) -> Result<String> {
        self.put_owned_symbol_birth(
            parent_history_hash,
            birth_seed,
            "record_field",
            owner_type_symbol,
        )
    }

    pub(crate) fn put_enum_variant_birth(
        &mut self,
        parent_history_hash: Option<&str>,
        owner_type_symbol: &str,
        birth_seed: &str,
    ) -> Result<String> {
        self.put_owned_symbol_birth(
            parent_history_hash,
            birth_seed,
            "enum_variant",
            owner_type_symbol,
        )
    }

    fn put_owned_symbol_birth(
        &mut self,
        parent_history_hash: Option<&str>,
        birth_seed: &str,
        symbol_kind: &str,
        owner_type_symbol: &str,
    ) -> Result<String> {
        self.put_object(
            "SymbolBirth",
            &json!({
                "symbol_kind": symbol_kind,
                "owner_type_symbol": owner_type_symbol,
                "birth_history_hash": parent_history_hash.unwrap_or("genesis"),
                "local_nonce": birth_seed,
            }),
        )
    }

    pub(crate) fn put_type_def(
        &mut self,
        type_symbol: &str,
        definition: &TypeDefinition,
    ) -> Result<String> {
        if definition.type_symbol() != type_symbol {
            bail!("type definition symbol does not match TypeDef symbol");
        }
        let (type_kind, definition_hash) = match definition {
            TypeDefinition::Record { .. } => ("record", self.put_record_def(definition)?),
            TypeDefinition::Enum { .. } => ("enum", self.put_enum_def(definition)?),
        };
        self.put_object(
            "TypeDef",
            &json!({
                "type_symbol": type_symbol,
                "type_kind": type_kind,
                "definition": definition_hash,
            }),
        )
    }

    pub(crate) fn put_record_def(&mut self, definition: &TypeDefinition) -> Result<String> {
        let TypeDefinition::Record {
            type_symbol,
            region_params,
            type_params,
            fields,
        } = definition
        else {
            bail!("put_record_def requires record definition");
        };
        validate_region_params(region_params)?;
        validate_type_params(type_params)?;
        validate_member_defs("record field", fields)?;
        // `type_params` is emitted only when non-empty so a non-generic record's
        // RecordDef payload (and hash) is unchanged from the pre-generics form.
        let mut payload = serde_json::Map::new();
        payload.insert("type_symbol".to_string(), json!(type_symbol));
        payload.insert(
            "region_params".to_string(),
            json!(
                region_params
                    .iter()
                    .map(|param| json!({ "region": param.region, "name": param.name }))
                    .collect::<Vec<_>>()
            ),
        );
        if !type_params.is_empty() {
            payload.insert(
                "type_params".to_string(),
                json!(
                    type_params
                        .iter()
                        .map(|param| json!({ "name": param.name }))
                        .collect::<Vec<_>>()
                ),
            );
        }
        payload.insert(
            "fields".to_string(),
            json!(
                fields
                    .iter()
                    .map(|field| {
                        json!({
                            "field_symbol": field.member_symbol,
                            "name": field.name,
                            "type": field.type_hash,
                        })
                    })
                    .collect::<Vec<_>>()
            ),
        );
        self.put_object("RecordDef", &JsonValue::Object(payload))
    }

    pub(crate) fn put_enum_def(&mut self, definition: &TypeDefinition) -> Result<String> {
        let TypeDefinition::Enum {
            type_symbol,
            region_params,
            type_params,
            variants,
        } = definition
        else {
            bail!("put_enum_def requires enum definition");
        };
        validate_region_params(region_params)?;
        validate_type_params(type_params)?;
        validate_member_defs("enum variant", variants)?;
        let mut payload = serde_json::Map::new();
        payload.insert("type_symbol".to_string(), json!(type_symbol));
        payload.insert(
            "region_params".to_string(),
            json!(
                region_params
                    .iter()
                    .map(|param| json!({ "region": param.region, "name": param.name }))
                    .collect::<Vec<_>>()
            ),
        );
        if !type_params.is_empty() {
            payload.insert(
                "type_params".to_string(),
                json!(
                    type_params
                        .iter()
                        .map(|param| json!({ "name": param.name }))
                        .collect::<Vec<_>>()
                ),
            );
        }
        payload.insert(
            "variants".to_string(),
            json!(
                variants
                    .iter()
                    .map(|variant| {
                        json!({
                            "variant_symbol": variant.member_symbol,
                            "name": variant.name,
                            "type": variant.type_hash,
                        })
                    })
                    .collect::<Vec<_>>()
            ),
        );
        self.put_object("EnumDef", &JsonValue::Object(payload))
    }

    pub(crate) fn type_definition(&self, type_def_hash: &str) -> Result<TypeDefinition> {
        if self.get_kind(type_def_hash)? != "TypeDef" {
            bail!("type definition hash points to non-TypeDef object {type_def_hash}");
        }
        let type_def = self.get_payload(type_def_hash)?;
        let type_symbol = type_def
            .get("type_symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("TypeDef missing type_symbol"))?
            .to_string();
        let type_kind = type_def
            .get("type_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("TypeDef missing type_kind"))?;
        let definition_hash = type_def
            .get("definition")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("TypeDef missing definition"))?;
        match type_kind {
            "record" => self.record_definition(&type_symbol, definition_hash),
            "enum" => self.enum_definition(&type_symbol, definition_hash),
            other => bail!("unknown TypeDef type_kind {other}"),
        }
    }

    pub(crate) fn record_definition(
        &self,
        expected_type_symbol: &str,
        record_def_hash: &str,
    ) -> Result<TypeDefinition> {
        if self.get_kind(record_def_hash)? != "RecordDef" {
            bail!("record definition hash points to non-RecordDef object {record_def_hash}");
        }
        let payload = self.get_payload(record_def_hash)?;
        let type_symbol = payload
            .get("type_symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("RecordDef missing type_symbol"))?
            .to_string();
        if type_symbol != expected_type_symbol {
            bail!("RecordDef type_symbol does not match TypeDef");
        }
        let region_params = region_params_from_payload(payload.get("region_params"))?;
        let type_params = type_params_from_payload(payload.get("type_params"))?;
        let fields =
            member_defs_from_payload("record field", "field_symbol", payload.get("fields"))?;
        validate_region_params(&region_params)?;
        validate_member_defs("record field", &fields)?;
        Ok(TypeDefinition::Record {
            type_symbol,
            region_params,
            type_params,
            fields,
        })
    }

    pub(crate) fn enum_definition(
        &self,
        expected_type_symbol: &str,
        enum_def_hash: &str,
    ) -> Result<TypeDefinition> {
        if self.get_kind(enum_def_hash)? != "EnumDef" {
            bail!("enum definition hash points to non-EnumDef object {enum_def_hash}");
        }
        let payload = self.get_payload(enum_def_hash)?;
        let type_symbol = payload
            .get("type_symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("EnumDef missing type_symbol"))?
            .to_string();
        if type_symbol != expected_type_symbol {
            bail!("EnumDef type_symbol does not match TypeDef");
        }
        let region_params = region_params_from_payload(payload.get("region_params"))?;
        let type_params = type_params_from_payload(payload.get("type_params"))?;
        let variants =
            member_defs_from_payload("enum variant", "variant_symbol", payload.get("variants"))?;
        validate_region_params(&region_params)?;
        validate_member_defs("enum variant", &variants)?;
        Ok(TypeDefinition::Enum {
            type_symbol,
            region_params,
            type_params,
            variants,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn resolve_type(&mut self, ty: &str) -> Result<String> {
        let parsed = parse_type_source(ty)?;
        self.put_type_spec(&parsed)
    }

    pub(crate) fn resolve_type_in_root(
        &mut self,
        current_module: &str,
        root: &ProgramRootPayload,
        ty: &str,
    ) -> Result<String> {
        self.resolve_type_in_root_with_regions(current_module, root, ty, &BTreeMap::new())
    }

    pub(crate) fn resolve_type_in_root_with_regions(
        &mut self,
        current_module: &str,
        root: &ProgramRootPayload,
        ty: &str,
        region_scope: &BTreeMap<String, String>,
    ) -> Result<String> {
        self.resolve_type_in_root_with_scope(current_module, root, ty, region_scope, &[])
    }

    /// Resolve a source type with both a region scope and a type-parameter scope
    /// (R11): a bare name matching `type_param_names[index]` binds to
    /// `TypeSpec::TypeParam { index }` before resolution, so a generic
    /// definition's members may reference its parameters. Empty `type_param_names`
    /// reproduces the non-generic `resolve_type_in_root_with_regions` behavior.
    pub(crate) fn resolve_type_in_root_with_scope(
        &mut self,
        current_module: &str,
        root: &ProgramRootPayload,
        ty: &str,
        region_scope: &BTreeMap<String, String>,
        type_param_names: &[String],
    ) -> Result<String> {
        let parsed = bind_type_params(parse_type_source(ty)?, type_param_names)?;
        self.put_type_spec_in_root(current_module, root, &parsed, region_scope)
    }

    #[allow(dead_code)]
    pub(crate) fn type_hash_for_source(&self, ty: &str) -> Result<String> {
        let parsed = parse_type_source(ty)?;
        type_hash_for_spec(&parsed)
    }

    #[allow(dead_code)]
    pub(crate) fn type_hash_for_source_in_root(
        &self,
        current_module: &str,
        root: &ProgramRootPayload,
        ty: &str,
    ) -> Result<String> {
        self.type_hash_for_source_in_root_with_regions(current_module, root, ty, &BTreeMap::new())
    }

    pub(crate) fn type_hash_for_source_in_root_with_regions(
        &self,
        current_module: &str,
        root: &ProgramRootPayload,
        ty: &str,
        region_scope: &BTreeMap<String, String>,
    ) -> Result<String> {
        self.type_hash_for_source_in_root_with_scope(
            current_module,
            root,
            ty,
            region_scope,
            &[],
        )
    }

    /// Hash a source type under a region scope and a type-parameter scope (R11) —
    /// the read-only twin of `resolve_type_in_root_with_scope`, used by the
    /// type-definition source-round-trip postcondition for generic types.
    pub(crate) fn type_hash_for_source_in_root_with_scope(
        &self,
        current_module: &str,
        root: &ProgramRootPayload,
        ty: &str,
        region_scope: &BTreeMap<String, String>,
        type_param_names: &[String],
    ) -> Result<String> {
        let parsed = bind_type_params(parse_type_source(ty)?, type_param_names)?;
        self.type_hash_for_parsed_in_root(current_module, root, &parsed, region_scope)
    }

    pub(crate) fn type_name(&self, hash: &str) -> Result<String> {
        if let Some(name) = scalar_int_source_name_for_hash(hash) {
            Ok(name)
        } else if hash == type_hash_for("Bool") {
            Ok("bool".to_string())
        } else if hash == type_hash_for("Unit") {
            Ok("unit".to_string())
        } else {
            self.type_spec(hash)?.to_source(self)
        }
    }

    pub(crate) fn type_name_with_regions(
        &self,
        hash: &str,
        region_names: &BTreeMap<String, String>,
    ) -> Result<String> {
        if let Some(name) = scalar_int_source_name_for_hash(hash) {
            return Ok(name);
        }
        if hash == type_hash_for("Bool") {
            return Ok("bool".to_string());
        }
        if hash == type_hash_for("Unit") {
            return Ok("unit".to_string());
        }
        match self.type_spec(hash)? {
            TypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let region_name = region_names
                    .get(&region)
                    .map(String::as_str)
                    .or_else(|| is_static_region(&region).then_some("static"))
                    .unwrap_or(region.as_str());
                let referent = self.type_name_with_regions(&referent, region_names)?;
                if mutable {
                    Ok(format!("&'{region_name} mut {referent}"))
                } else {
                    Ok(format!("&'{region_name} {referent}"))
                }
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                let pointee = self.type_name_with_regions(&pointee, region_names)?;
                if mutable {
                    Ok(format!("raw_mut_ptr<{pointee}>"))
                } else {
                    Ok(format!("raw_ptr<{pointee}>"))
                }
            }
            TypeSpec::Box { element } => {
                let element = self.type_name_with_regions(&element, region_names)?;
                Ok(format!("box<{element}>"))
            }
            TypeSpec::Vec { element } => {
                let element = self.type_name_with_regions(&element, region_names)?;
                Ok(format!("vec<{element}>"))
            }
            TypeSpec::String => Ok("string".to_string()),
            TypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let region_name = region_names
                    .get(&region)
                    .map(String::as_str)
                    .or_else(|| is_static_region(&region).then_some("static"))
                    .unwrap_or(region.as_str());
                let element = self.type_name_with_regions(&element, region_names)?;
                if mutable {
                    Ok(format!("mut_slice<'{region_name}, {element}>"))
                } else {
                    Ok(format!("slice<'{region_name}, {element}>"))
                }
            }
            TypeSpec::FixedArray { element, len } => Ok(format!(
                "array<{}, {len}>",
                self.type_name_with_regions(&element, region_names)?
            )),
            TypeSpec::Record(fields) => {
                let rendered = fields
                    .iter()
                    .map(|field| {
                        Ok(format!(
                            "{}: {}",
                            field.name,
                            self.type_name_with_regions(&field.type_hash, region_names)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("record {{{}}}", rendered.join(", ")))
            }
            TypeSpec::Enum(variants) => {
                let rendered = variants
                    .iter()
                    .map(|variant| {
                        Ok(format!(
                            "{}: {}",
                            variant.name,
                            self.type_name_with_regions(&variant.type_hash, region_names)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("enum {{{}}}", rendered.join(", ")))
            }
            other => other.to_source(self),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn type_name_in_root(
        &self,
        root: &ProgramRootPayload,
        current_module: &str,
        hash: &str,
    ) -> Result<String> {
        self.type_name_in_root_with_regions(root, current_module, hash, &BTreeMap::new())
    }

    /// Render the *constructor head* of an enum type at a `Enum::variant(..)` site:
    /// the base name plus any region arguments, but WITHOUT type arguments (R11).
    /// A generic enum instance projects as the bare `Option` (not `Option<i64>`),
    /// and the type arguments are re-inferred when the projection is re-parsed in
    /// the same context — keeping the construction grammar free of `<...>` at `::`
    /// while staying a byte-stable round trip. Non-generic and region-only enums
    /// render exactly as before.
    pub(crate) fn enum_constructor_type_source(
        &self,
        root: &ProgramRootPayload,
        current_module: &str,
        enum_type_hash: &str,
        region_names: &BTreeMap<String, String>,
    ) -> Result<String> {
        match self.type_spec(enum_type_hash)? {
            TypeSpec::Named {
                type_symbol,
                region_args,
                ..
            } => {
                let mut source =
                    self.type_symbol_display_for_module(root, current_module, &type_symbol)?;
                if !region_args.is_empty() {
                    let args = region_args
                        .iter()
                        .map(|region| {
                            region_names
                                .get(region)
                                .map(|name| format!("'{name}"))
                                .unwrap_or_else(|| region.clone())
                        })
                        .collect::<Vec<_>>();
                    source.push_str(&format!("<{}>", args.join(", ")));
                }
                Ok(source)
            }
            _ => self.type_name_in_root_with_regions(
                root,
                current_module,
                enum_type_hash,
                region_names,
            ),
        }
    }

    pub(crate) fn type_name_in_root_with_regions(
        &self,
        root: &ProgramRootPayload,
        current_module: &str,
        hash: &str,
        region_names: &BTreeMap<String, String>,
    ) -> Result<String> {
        self.type_name_in_root_with_scope(root, current_module, hash, region_names, &[])
    }

    /// Root-aware source rendering with both a region-name scope and a
    /// type-parameter-name scope (R11). `type_param_names[index]` is the display
    /// name of the enclosing generic definition's `index`-th parameter, so a
    /// `TypeSpec::TypeParam { index }` renders as that name and a generic instance
    /// `Named { type_args }` renders its arguments. Non-generic callers reach this
    /// through `type_name_in_root_with_regions` (empty type-parameter scope).
    pub(crate) fn type_name_in_root_with_scope(
        &self,
        root: &ProgramRootPayload,
        current_module: &str,
        hash: &str,
        region_names: &BTreeMap<String, String>,
        type_param_names: &[String],
    ) -> Result<String> {
        if let Some(name) = scalar_int_source_name_for_hash(hash) {
            return Ok(name);
        }
        if hash == type_hash_for("Bool") {
            return Ok("bool".to_string());
        }
        if hash == type_hash_for("Unit") {
            return Ok("unit".to_string());
        }
        let recurse = |this: &Self, inner: &str| {
            this.type_name_in_root_with_scope(
                root,
                current_module,
                inner,
                region_names,
                type_param_names,
            )
        };
        match self.type_spec(hash)? {
            TypeSpec::Named {
                type_symbol,
                region_args,
                type_args,
            } => {
                let mut source =
                    self.type_symbol_display_for_module(root, current_module, &type_symbol)?;
                if !region_args.is_empty() || !type_args.is_empty() {
                    // Region arguments (`'r`) render first, then type arguments —
                    // the order `parse_optional_type_args` accepts, so the
                    // projection re-parses to the same type.
                    let mut args = region_args
                        .iter()
                        .map(|region| {
                            region_names
                                .get(region)
                                .map(|name| format!("'{name}"))
                                .unwrap_or_else(|| region.clone())
                        })
                        .collect::<Vec<_>>();
                    for arg in &type_args {
                        args.push(recurse(self, arg)?);
                    }
                    source.push_str(&format!("<{}>", args.join(", ")));
                }
                Ok(source)
            }
            TypeSpec::TypeParam { index } => type_param_names
                .get(index as usize)
                .cloned()
                .ok_or_else(|| anyhow!("type parameter {index} out of scope while rendering")),
            TypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let region_name = region_names
                    .get(&region)
                    .map(String::as_str)
                    .or_else(|| is_static_region(&region).then_some("static"))
                    .unwrap_or(region.as_str());
                let referent = recurse(self, &referent)?;
                if mutable {
                    Ok(format!("&'{region_name} mut {referent}"))
                } else {
                    Ok(format!("&'{region_name} {referent}"))
                }
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                let pointee = recurse(self, &pointee)?;
                if mutable {
                    Ok(format!("raw_mut_ptr<{pointee}>"))
                } else {
                    Ok(format!("raw_ptr<{pointee}>"))
                }
            }
            TypeSpec::Box { element } => Ok(format!("box<{}>", recurse(self, &element)?)),
            TypeSpec::Vec { element } => Ok(format!("vec<{}>", recurse(self, &element)?)),
            TypeSpec::String => Ok("string".to_string()),
            TypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let region_name = region_names
                    .get(&region)
                    .map(String::as_str)
                    .or_else(|| is_static_region(&region).then_some("static"))
                    .unwrap_or(region.as_str());
                let element = recurse(self, &element)?;
                if mutable {
                    Ok(format!("mut_slice<'{region_name}, {element}>"))
                } else {
                    Ok(format!("slice<'{region_name}, {element}>"))
                }
            }
            TypeSpec::FixedArray { element, len } => {
                Ok(format!("array<{}, {len}>", recurse(self, &element)?))
            }
            TypeSpec::Record(fields) => {
                let rendered = fields
                    .iter()
                    .map(|field| Ok(format!("{}: {}", field.name, recurse(self, &field.type_hash)?)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("record {{{}}}", rendered.join(", ")))
            }
            TypeSpec::Enum(variants) => {
                let rendered = variants
                    .iter()
                    .map(|variant| {
                        Ok(format!(
                            "{}: {}",
                            variant.name,
                            recurse(self, &variant.type_hash)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("enum {{{}}}", rendered.join(", ")))
            }
            TypeSpec::Builtin(_) => self.type_name(hash),
        }
    }

    pub(crate) fn type_spec(&self, hash: &str) -> Result<TypeSpec> {
        if let Some(int) = SCALAR_INT_TYPES.iter().find(|t| hash == type_hash_for(t.name)) {
            return Ok(TypeSpec::Builtin(int.name.to_string()));
        }
        if hash == type_hash_for("Bool") {
            return Ok(TypeSpec::Builtin("Bool".to_string()));
        }
        if hash == type_hash_for("Unit") {
            return Ok(TypeSpec::Builtin("Unit".to_string()));
        }
        if self.get_kind(hash)? != "Type" {
            bail!("type hash points to non-Type object {hash}");
        }
        type_spec_from_payload(&self.get_payload(hash)?)
    }

    pub(crate) fn type_spec_in_root(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<TypeSpec> {
        match self.type_spec(type_hash)? {
            TypeSpec::Named {
                type_symbol,
                region_args,
                type_args,
            } => {
                let (definition, region_substitutions, param_args) = self
                    .named_type_definition_with_args(
                        root,
                        &type_symbol,
                        &region_args,
                        &type_args,
                    )?;
                match definition {
                    TypeDefinition::Record { fields, .. } => Ok(TypeSpec::Record(
                        fields
                            .into_iter()
                            .map(|field| {
                                Ok(TypeFieldSpec {
                                    name: field.name,
                                    type_hash: self.substitute_type_hash(
                                        &field.type_hash,
                                        &region_substitutions,
                                        &param_args,
                                    )?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?,
                    )),
                    TypeDefinition::Enum { variants, .. } => Ok(TypeSpec::Enum(
                        variants
                            .into_iter()
                            .map(|variant| {
                                Ok(TypeFieldSpec {
                                    name: variant.name,
                                    type_hash: self.substitute_type_hash(
                                        &variant.type_hash,
                                        &region_substitutions,
                                        &param_args,
                                    )?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?,
                    )),
                }
            }
            other => Ok(other),
        }
    }

    /// Resolve a named type's definition plus the substitutions for instantiating
    /// it: a region map (from region parameters) and a positional type-argument
    /// vector (from type parameters, R11). Both arities are checked, so a generic
    /// `Option<i64>` used with the wrong number of type arguments fails closed.
    fn named_type_definition_with_args(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        region_args: &[String],
        type_args: &[String],
    ) -> Result<(TypeDefinition, BTreeMap<String, String>, Vec<String>)> {
        let entry = self
            .root_type(root, type_symbol)
            .ok_or_else(|| anyhow!("named type missing from root {type_symbol}"))?;
        let definition = self.type_definition(&entry.type_def)?;
        if definition.region_params().len() != region_args.len() {
            bail!(
                "named type {type_symbol} expects {} region args, got {}",
                definition.region_params().len(),
                region_args.len()
            );
        }
        if definition.type_params().len() != type_args.len() {
            bail!(
                "named type {type_symbol} expects {} type args, got {}",
                definition.type_params().len(),
                type_args.len()
            );
        }
        let region_substitutions = definition
            .region_params()
            .iter()
            .zip(region_args.iter())
            .map(|(param, arg)| (param.region.clone(), arg.clone()))
            .collect();
        Ok((definition, region_substitutions, type_args.to_vec()))
    }

    fn substitute_region_hash(
        &self,
        region: String,
        region_substitutions: &BTreeMap<String, String>,
    ) -> String {
        region_substitutions.get(&region).cloned().unwrap_or(region)
    }

    fn substitute_type_regions_hash(
        &self,
        type_hash: &str,
        region_substitutions: &BTreeMap<String, String>,
    ) -> Result<String> {
        self.substitute_type_hash(type_hash, region_substitutions, &[])
    }

    /// Hash-only substitution of a (possibly generic) template type: regions via
    /// `region_substitutions`, and a `TypeSpec::TypeParam { index }` via
    /// `param_args[index]` (R11). A nested generic instance's own `type_args` are
    /// substituted in turn, so `box<Pair<T>>` with `T := i64` becomes
    /// `box<Pair<i64>>`. Returns the substituted type's content hash without
    /// storing intermediate Type objects (used by type resolution / checking);
    /// `put_substituted_type` is the storing twin used when the result must be
    /// referenceable by layout and lowering.
    fn substitute_type_hash(
        &self,
        type_hash: &str,
        region_substitutions: &BTreeMap<String, String>,
        param_args: &[String],
    ) -> Result<String> {
        if region_substitutions.is_empty() && param_args.is_empty() {
            return Ok(type_hash.to_string());
        }
        let recurse = |this: &Self, inner: &str| {
            this.substitute_type_hash(inner, region_substitutions, param_args)
        };
        match self.type_spec(type_hash)? {
            TypeSpec::Builtin(_) => Ok(type_hash.to_string()),
            TypeSpec::TypeParam { index } => param_args
                .get(index as usize)
                .cloned()
                .ok_or_else(|| anyhow!("type parameter {index} has no substitution argument")),
            TypeSpec::Named {
                type_symbol,
                region_args,
                type_args,
            } => hash_for_type_spec(&TypeSpec::Named {
                type_symbol,
                region_args: region_args
                    .into_iter()
                    .map(|region| self.substitute_region_hash(region, region_substitutions))
                    .collect(),
                type_args: type_args
                    .iter()
                    .map(|arg| recurse(self, arg))
                    .collect::<Result<Vec<_>>>()?,
            }),
            TypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let referent = recurse(self, &referent)?;
                hash_for_type_spec(&TypeSpec::Reference {
                    region: self.substitute_region_hash(region, region_substitutions),
                    mutable,
                    referent,
                })
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                let pointee = recurse(self, &pointee)?;
                hash_for_type_spec(&TypeSpec::RawPointer { mutable, pointee })
            }
            TypeSpec::Box { element } => {
                let element = recurse(self, &element)?;
                hash_for_type_spec(&TypeSpec::Box { element })
            }
            TypeSpec::Vec { element } => {
                let element = recurse(self, &element)?;
                hash_for_type_spec(&TypeSpec::Vec { element })
            }
            TypeSpec::String => hash_for_type_spec(&TypeSpec::String),
            TypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let element = recurse(self, &element)?;
                hash_for_type_spec(&TypeSpec::Slice {
                    region: self.substitute_region_hash(region, region_substitutions),
                    mutable,
                    element,
                })
            }
            TypeSpec::FixedArray { element, len } => {
                let element = recurse(self, &element)?;
                hash_for_type_spec(&TypeSpec::FixedArray { element, len })
            }
            TypeSpec::Record(fields) => {
                let fields = fields
                    .into_iter()
                    .map(|field| {
                        Ok(TypeFieldSpec {
                            name: field.name,
                            type_hash: recurse(self, &field.type_hash)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                hash_for_type_spec(&TypeSpec::Record(fields))
            }
            TypeSpec::Enum(variants) => {
                let variants = variants
                    .into_iter()
                    .map(|variant| {
                        Ok(TypeFieldSpec {
                            name: variant.name,
                            type_hash: recurse(self, &variant.type_hash)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                hash_for_type_spec(&TypeSpec::Enum(variants))
            }
        }
    }

    fn put_substituted_type_regions(
        &mut self,
        type_hash: &str,
        region_substitutions: &BTreeMap<String, String>,
    ) -> Result<String> {
        self.put_substituted_type(type_hash, region_substitutions, &[])
    }

    /// Storing twin of `substitute_type_hash`: substitutes regions and type
    /// parameters (R11) and persists every intermediate structural Type object,
    /// so the result — and its children — can be loaded by layout and lowering.
    fn put_substituted_type(
        &mut self,
        type_hash: &str,
        region_substitutions: &BTreeMap<String, String>,
        param_args: &[String],
    ) -> Result<String> {
        if region_substitutions.is_empty() && param_args.is_empty() {
            return Ok(type_hash.to_string());
        }
        match self.type_spec(type_hash)? {
            TypeSpec::Builtin(_) => Ok(type_hash.to_string()),
            TypeSpec::TypeParam { index } => param_args
                .get(index as usize)
                .cloned()
                .ok_or_else(|| anyhow!("type parameter {index} has no substitution argument")),
            TypeSpec::Named {
                type_symbol,
                region_args,
                type_args,
            } => {
                let region_args = region_args
                    .into_iter()
                    .map(|region| self.substitute_region_hash(region, region_substitutions))
                    .collect();
                let type_args = type_args
                    .iter()
                    .map(|arg| self.put_substituted_type(arg, region_substitutions, param_args))
                    .collect::<Result<Vec<_>>>()?;
                self.put_structural_type(TypeSpec::Named {
                    type_symbol,
                    region_args,
                    type_args,
                })
            }
            TypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let referent =
                    self.put_substituted_type(&referent, region_substitutions, param_args)?;
                self.put_structural_type(TypeSpec::Reference {
                    region: self.substitute_region_hash(region, region_substitutions),
                    mutable,
                    referent,
                })
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                let pointee =
                    self.put_substituted_type(&pointee, region_substitutions, param_args)?;
                self.put_structural_type(TypeSpec::RawPointer { mutable, pointee })
            }
            TypeSpec::Box { element } => {
                let element =
                    self.put_substituted_type(&element, region_substitutions, param_args)?;
                self.put_structural_type(TypeSpec::Box { element })
            }
            TypeSpec::Vec { element } => {
                let element =
                    self.put_substituted_type(&element, region_substitutions, param_args)?;
                self.put_structural_type(TypeSpec::Vec { element })
            }
            TypeSpec::String => self.put_structural_type(TypeSpec::String),
            TypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let element =
                    self.put_substituted_type(&element, region_substitutions, param_args)?;
                self.put_structural_type(TypeSpec::Slice {
                    region: self.substitute_region_hash(region, region_substitutions),
                    mutable,
                    element,
                })
            }
            TypeSpec::FixedArray { element, len } => {
                let element =
                    self.put_substituted_type(&element, region_substitutions, param_args)?;
                self.put_structural_type(TypeSpec::FixedArray { element, len })
            }
            TypeSpec::Record(fields) => {
                let fields = fields
                    .into_iter()
                    .map(|field| {
                        Ok(TypeFieldSpec {
                            name: field.name,
                            type_hash: self.put_substituted_type(
                                &field.type_hash,
                                region_substitutions,
                                param_args,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.put_structural_type(TypeSpec::Record(fields))
            }
            TypeSpec::Enum(variants) => {
                let variants = variants
                    .into_iter()
                    .map(|variant| {
                        Ok(TypeFieldSpec {
                            name: variant.name,
                            type_hash: self.put_substituted_type(
                                &variant.type_hash,
                                region_substitutions,
                                param_args,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.put_structural_type(TypeSpec::Enum(variants))
            }
        }
    }

    fn materialize_named_type_expansion(
        &mut self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        region_args: &[String],
        type_args: &[String],
    ) -> Result<()> {
        // A plain named type (no region or type arguments) has nothing to
        // substitute — its members are already stored as written, so skip the
        // walk entirely (preserving the pre-generics no-op fast path).
        if region_args.is_empty() && type_args.is_empty() {
            return Ok(());
        }
        let mut seen = std::collections::BTreeSet::new();
        self.materialize_instance(root, type_symbol, region_args, type_args, &mut seen, 0)
    }

    /// Eagerly store the substituted member types of one named-type instantiation,
    /// and recurse into any *nested* generic instances it produces (R11). A
    /// generic substitution changes type structure (`Pair<T>` → `Pair<i64>`), so a
    /// nested instance reached only through substitution — never written in source
    /// — would otherwise have no stored expansion for layout/lowering to load. The
    /// `seen` set keeps a recursive generic (`List<T>` whose `cons` holds a
    /// `box<List<T>>`) terminating: the box also breaks the layout size cycle.
    /// `seen` does NOT terminate an instantiation whose arguments GROW each level
    /// (`Grow<T>` containing `Grow<box<T>>` — every instance hash is new), so
    /// `depth` caps the chain fail-closed (#7) instead of overflowing the host
    /// stack.
    fn materialize_instance(
        &mut self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        region_args: &[String],
        type_args: &[String],
        seen: &mut std::collections::BTreeSet<String>,
        depth: usize,
    ) -> Result<()> {
        if depth > GENERIC_INSTANTIATION_DEPTH_LIMIT {
            bail!(
                "generic type instantiation exceeds the depth limit ({GENERIC_INSTANTIATION_DEPTH_LIMIT}) at {type_symbol}: a recursive generic whose type arguments grow each level (polymorphic recursion) does not converge"
            );
        }
        let instance_hash = self.put_structural_type(TypeSpec::Named {
            type_symbol: type_symbol.to_string(),
            region_args: region_args.to_vec(),
            type_args: type_args.to_vec(),
        })?;
        if !seen.insert(instance_hash) {
            return Ok(());
        }
        let (definition, region_substitutions, param_args) =
            self.named_type_definition_with_args(root, type_symbol, region_args, type_args)?;
        let members = match &definition {
            TypeDefinition::Record { fields, .. } => fields.clone(),
            TypeDefinition::Enum { variants, .. } => variants.clone(),
        };
        for member in members {
            let substituted =
                self.put_substituted_type(&member.type_hash, &region_substitutions, &param_args)?;
            self.materialize_nested_instances(root, &substituted, seen, depth + 1)?;
        }
        Ok(())
    }

    fn materialize_nested_instances(
        &mut self,
        root: &ProgramRootPayload,
        type_hash: &str,
        seen: &mut std::collections::BTreeSet<String>,
        depth: usize,
    ) -> Result<()> {
        match self.type_spec(type_hash)? {
            TypeSpec::Named {
                type_symbol,
                region_args,
                type_args,
            } => {
                if !type_args.is_empty() {
                    self.materialize_instance(
                        root,
                        &type_symbol,
                        &region_args,
                        &type_args,
                        seen,
                        depth,
                    )?;
                }
                Ok(())
            }
            TypeSpec::Box { element }
            | TypeSpec::Vec { element }
            | TypeSpec::FixedArray { element, .. }
            | TypeSpec::Slice { element, .. }
            | TypeSpec::Reference {
                referent: element, ..
            }
            | TypeSpec::RawPointer { pointee: element, .. } => {
                self.materialize_nested_instances(root, &element, seen, depth)
            }
            TypeSpec::Record(members) | TypeSpec::Enum(members) => {
                for member in members {
                    self.materialize_nested_instances(root, &member.type_hash, seen, depth)?;
                }
                Ok(())
            }
            TypeSpec::Builtin(_) | TypeSpec::String | TypeSpec::TypeParam { .. } => Ok(()),
        }
    }

    pub(crate) fn record_field_type_in_root(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        field: &str,
    ) -> Result<String> {
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Record(fields) => fields
                .into_iter()
                .find(|candidate| candidate.name == field)
                .map(|candidate| candidate.type_hash)
                .ok_or_else(|| anyhow!("record has no field {field}")),
            other => bail!(
                "field access requires record type, got {}",
                other.to_source(self)?
            ),
        }
    }

    /// Resolve the concrete enum *instance* type at a construction site
    /// `Enum::variant(value)` (R11). For a non-generic enum, or one written with
    /// explicit type arguments (`Option<i64>::some(..)`), this is ordinary
    /// resolution. For a bare generic enum (`Option::some(5)`), the type arguments
    /// are inferred: from the `expected_type` when it is an instance of the same
    /// generic, otherwise by matching the variant's payload template against the
    /// payload value's concrete type.
    #[allow(clippy::too_many_arguments)]
    fn resolve_enum_construct_type(
        &mut self,
        current_module: &str,
        root: &ProgramRootPayload,
        enum_type: &str,
        variant: &str,
        value: &RawExpr,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
        expected_type: Option<&str>,
    ) -> Result<String> {
        // Inspect the written type: its base name and whether explicit arguments
        // were given. A non-named (inline `enum { .. }`) construction type is never
        // generic, so it resolves directly.
        let parsed = parse_type_source(enum_type)?;
        let (base_name, has_explicit_args) = match &parsed {
            ParsedTypeSpec::Named {
                name,
                region_args,
                type_args,
            } => (name.clone(), !region_args.is_empty() || !type_args.is_empty()),
            _ => {
                return self.resolve_type_in_root_with_regions(
                    current_module,
                    root,
                    enum_type,
                    region_scope,
                );
            }
        };
        let Some(base_symbol) = resolve_named_type_in_root(root, current_module, &base_name) else {
            // Not a known named type (could be a module-qualified inline form the
            // direct resolver still understands) — defer to ordinary resolution.
            return self.resolve_type_in_root_with_regions(
                current_module,
                root,
                enum_type,
                region_scope,
            );
        };
        let entry = self
            .root_type(root, &base_symbol)
            .ok_or_else(|| anyhow!("type {base_name} missing root definition"))?;
        let definition = self.type_definition(&entry.type_def)?;
        let type_param_count = definition.type_params().len();

        // Non-generic, or the writer supplied explicit arguments: resolve directly.
        if type_param_count == 0 || has_explicit_args {
            return self.resolve_type_in_root_with_regions(
                current_module,
                root,
                enum_type,
                region_scope,
            );
        }
        if !definition.region_params().is_empty() {
            bail!(
                "generic enum {base_name} with region parameters requires explicit type arguments at construction"
            );
        }

        // Infer the type arguments. Prefer an `expected_type` that is an instance
        // of this same generic.
        let mut inferred: Option<Vec<String>> = None;
        if let Some(expected) = expected_type
            && let TypeSpec::Named {
                type_symbol,
                type_args,
                ..
            } = self.type_spec(expected)?
            && type_symbol == base_symbol
            && type_args.len() == type_param_count
        {
            inferred = Some(type_args);
        }

        let instance_args = match inferred {
            Some(args) => args,
            None => {
                // Match the variant's payload template against the payload's type.
                let template = self.generic_variant_payload_template(root, &base_symbol, variant)?;
                let probe = self.type_expr_with_locals(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let mut solutions = vec![None; type_param_count];
                self.infer_type_args_from_match(&template, &probe.type_hash, &mut solutions)?;
                solutions
                    .into_iter()
                    .enumerate()
                    .map(|(idx, slot)| {
                        slot.ok_or_else(|| {
                            anyhow!(
                                "cannot infer type argument {idx} for generic enum {base_name}; \
                                 write it explicitly, e.g. {base_name}<...>::{variant}(...)"
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };

        let instance = self.put_structural_type(TypeSpec::Named {
            type_symbol: base_symbol.clone(),
            region_args: Vec::new(),
            type_args: instance_args.clone(),
        })?;
        self.materialize_named_type_expansion(root, &base_symbol, &[], &instance_args)?;
        Ok(instance)
    }

    /// The (un-substituted) payload type of `variant` in the *generic template* of
    /// the enum named by `type_symbol` (R11) — i.e. the type that still contains
    /// `TypeSpec::TypeParam`s. Used to infer a generic enum instance's type
    /// arguments at a construction site.
    fn generic_variant_payload_template(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        variant: &str,
    ) -> Result<String> {
        let entry = self
            .root_type(root, type_symbol)
            .ok_or_else(|| anyhow!("named type missing from root {type_symbol}"))?;
        let definition = self.type_definition(&entry.type_def)?;
        let TypeDefinition::Enum { variants, .. } = definition else {
            bail!("enum variant construction requires enum type");
        };
        variants
            .into_iter()
            .find(|candidate| candidate.name == variant)
            .map(|candidate| candidate.type_hash)
            .ok_or_else(|| anyhow!("enum has no variant {variant}"))
    }

    /// Unify a generic template type `template` (which may contain `TypeParam`s)
    /// against a concrete type `concrete`, recording each parameter's solution in
    /// `solutions[index]` (R11). A parameter bound twice to differing types is a
    /// conflict. Shapes that do not line up simply leave parameters unsolved (the
    /// caller reports "cannot infer"); this is matching, not full unification.
    fn infer_type_args_from_match(
        &self,
        template: &str,
        concrete: &str,
        solutions: &mut [Option<String>],
    ) -> Result<()> {
        match self.type_spec(template)? {
            TypeSpec::TypeParam { index } => {
                let slot = solutions
                    .get_mut(index as usize)
                    .ok_or_else(|| anyhow!("type parameter {index} out of range"))?;
                match slot {
                    Some(existing) if existing != concrete => bail!(
                        "conflicting type arguments inferred for parameter {index}"
                    ),
                    _ => *slot = Some(concrete.to_string()),
                }
                Ok(())
            }
            TypeSpec::Box { element } => {
                if let TypeSpec::Box { element: c } = self.type_spec(concrete)? {
                    self.infer_type_args_from_match(&element, &c, solutions)?;
                }
                Ok(())
            }
            TypeSpec::Vec { element } => {
                if let TypeSpec::Vec { element: c } = self.type_spec(concrete)? {
                    self.infer_type_args_from_match(&element, &c, solutions)?;
                }
                Ok(())
            }
            TypeSpec::FixedArray { element, .. } => {
                if let TypeSpec::FixedArray { element: c, .. } = self.type_spec(concrete)? {
                    self.infer_type_args_from_match(&element, &c, solutions)?;
                }
                Ok(())
            }
            TypeSpec::Slice { element, .. } => {
                if let TypeSpec::Slice { element: c, .. } = self.type_spec(concrete)? {
                    self.infer_type_args_from_match(&element, &c, solutions)?;
                }
                Ok(())
            }
            TypeSpec::Reference { referent, .. } => {
                if let TypeSpec::Reference { referent: c, .. } = self.type_spec(concrete)? {
                    self.infer_type_args_from_match(&referent, &c, solutions)?;
                }
                Ok(())
            }
            TypeSpec::RawPointer { pointee, .. } => {
                if let TypeSpec::RawPointer { pointee: c, .. } = self.type_spec(concrete)? {
                    self.infer_type_args_from_match(&pointee, &c, solutions)?;
                }
                Ok(())
            }
            TypeSpec::Named { type_args, .. } => {
                if let TypeSpec::Named {
                    type_args: c_args, ..
                } = self.type_spec(concrete)?
                    && c_args.len() == type_args.len()
                {
                    for (t, c) in type_args.iter().zip(c_args.iter()) {
                        self.infer_type_args_from_match(t, c, solutions)?;
                    }
                }
                Ok(())
            }
            TypeSpec::Record(fields) => {
                if let TypeSpec::Record(c_fields) = self.type_spec(concrete)?
                    && c_fields.len() == fields.len()
                {
                    for (t, c) in fields.iter().zip(c_fields.iter()) {
                        self.infer_type_args_from_match(&t.type_hash, &c.type_hash, solutions)?;
                    }
                }
                Ok(())
            }
            TypeSpec::Enum(variants) => {
                if let TypeSpec::Enum(c_variants) = self.type_spec(concrete)?
                    && c_variants.len() == variants.len()
                {
                    for (t, c) in variants.iter().zip(c_variants.iter()) {
                        self.infer_type_args_from_match(&t.type_hash, &c.type_hash, solutions)?;
                    }
                }
                Ok(())
            }
            TypeSpec::Builtin(_) | TypeSpec::String => Ok(()),
        }
    }

    /// Resolve a call's expected parameter and return types, substituting any
    /// type arguments recorded on a generic call (R11) so verification checks
    /// against the concrete instantiation rather than the opaque template. A
    /// non-generic call (no `type_args`) returns the signature parts unchanged.
    fn call_signature_with_type_args(
        &self,
        signature_hash: &str,
        payload: &JsonValue,
    ) -> Result<(Vec<String>, String)> {
        let (params, return_type) = self.signature_parts(signature_hash)?;
        let type_args = call_type_args(payload)?;
        if type_args.is_empty() {
            return Ok((params, return_type));
        }
        let no_regions = BTreeMap::new();
        let params = params
            .iter()
            .map(|param| self.substitute_type_hash(param, &no_regions, &type_args))
            .collect::<Result<Vec<_>>>()?;
        let return_type = self.substitute_type_hash(&return_type, &no_regions, &type_args)?;
        Ok((params, return_type))
    }

    /// Build a partial type-argument substitution from a solution vector (R11):
    /// a solved parameter maps to its inferred type, an unsolved one maps to its
    /// own `TypeParam` (so substituting leaves it in place). Substituting a
    /// parameter type with this yields a concrete type exactly when every
    /// parameter it mentions is already solved — the test the deferred-argument
    /// retry uses to know an anchor is usable.
    fn partial_type_args(&self, solutions: &[Option<String>]) -> Result<Vec<String>> {
        solutions
            .iter()
            .enumerate()
            .map(|(index, slot)| match slot {
                Some(hash) => Ok(hash.clone()),
                None => hash_for_type_spec(&TypeSpec::TypeParam {
                    index: index as u32,
                }),
            })
            .collect()
    }

    /// Materialize the structural expansion of every generic instance nested in
    /// `type_hash` (R11), so a type produced only by substitution at a call
    /// site (e.g. `Option<i64>` returned by a generic function) has its members
    /// stored for the caller's layout and lowering to load.
    fn materialize_type_instances(
        &mut self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        self.materialize_nested_instances(root, type_hash, &mut seen, 0)
    }

    /// Type-check a call to a generic function (R11): infer the callee's type
    /// arguments from the argument types (and the expected result type), verify
    /// the now-concrete parameter types, and record the inferred `type_args` on
    /// the call so lowering can monomorphize it. The typed call still names the
    /// generic symbol — its monomorphic instance is materialized as a derived
    /// root symbol by `monomorphize_into_root` — so the reference evaluator runs
    /// the (type-erased) generic body unchanged while the native backend lowers
    /// one concrete instance per `type_args`.
    #[allow(clippy::too_many_arguments)]
    fn type_generic_call(
        &mut self,
        current_module: &str,
        name: &str,
        symbol: &str,
        expected_params: &[String],
        return_type: &str,
        type_param_count: usize,
        callee_regions: &BTreeSet<String>,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
        expected_type: Option<&str>,
    ) -> Result<TypeCheckResult> {
        // 1. Type-check the arguments. A generic parameter type is opaque, so
        //    in the first pass each argument is typed on its own (no
        //    expectation). An argument that needs its parameter type to type —
        //    e.g. a bare `Option::none` at parameter `Option<T>` — fails here
        //    and is retried in pass 3 once `T` is solved from the other
        //    arguments.
        let mut pass1 = Vec::with_capacity(args.len());
        for arg in args {
            pass1.push(self.type_expr_with_locals(
                current_module,
                arg,
                root,
                param_names,
                param_types,
                region_scope,
                locals,
            ));
        }
        // 2. Solve each type parameter by matching the parameter template
        //    against every argument that typed, then fall back to the expected
        //    result type (so a call whose only `T`-bearing argument needs
        //    context still resolves).
        let mut solutions = vec![None; type_param_count];
        for (idx, typed) in pass1.iter().enumerate() {
            if let Ok(typed) = typed {
                self.infer_type_args_from_match(
                    &expected_params[idx],
                    &typed.type_hash,
                    &mut solutions,
                )?;
            }
        }
        if solutions.iter().any(Option::is_none)
            && let Some(expected_outer) = expected_type
        {
            self.infer_type_args_from_match(return_type, expected_outer, &mut solutions)?;
        }
        // 3. Retry any argument that did not type on its own, now anchoring it
        //    to its parameter type with the solved arguments substituted in. If
        //    that anchor is still not concrete the type arguments are genuinely
        //    under-determined.
        let partial_args = self.partial_type_args(&solutions)?;
        let no_regions = BTreeMap::new();
        let mut typed_args = Vec::with_capacity(args.len());
        let mut arg_types = Vec::with_capacity(args.len());
        for (idx, typed) in pass1.into_iter().enumerate() {
            let typed = match typed {
                Ok(typed) => typed,
                Err(original) => {
                    // The storing twin: the anchor can be a NEW type (`&'r T` at
                    // T=i64 mints `&'r i64`), and typing the argument against it
                    // loads it — an unstored hash is a `missing object` (#11).
                    let anchor =
                        self.put_substituted_type(&expected_params[idx], &no_regions, &partial_args)?;
                    if !self.type_is_concrete(&anchor)? {
                        return Err(original.context(format!(
                            "cannot infer the type arguments of generic function {name} for \
                             argument {idx}; annotate the call's context"
                        )));
                    }
                    self.type_expr_with_locals_expecting(
                        current_module,
                        &args[idx],
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                        Some(&anchor),
                    )?
                }
            };
            arg_types.push(typed.type_hash);
            typed_args.push(typed.expr_hash);
        }
        let type_args = solutions
            .into_iter()
            .enumerate()
            .map(|(idx, slot)| {
                slot.ok_or_else(|| {
                    anyhow!(
                        "cannot infer type argument {idx} for generic function {name}; \
                         the argument types do not determine it (annotate the call's context)"
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // 4. Substitute the solved type arguments into the parameter and return
        //    types, infer any region arguments against the now-concrete
        //    parameter types, and check each argument is assignable.
        let mut region_substitutions = BTreeMap::new();
        for (idx, expected) in expected_params.iter().enumerate() {
            // The storing twin, not `substitute_type_hash`: substitution can mint
            // a type that exists nowhere else (`&'r T` at T=i64 → `&'r i64`), and
            // the assignability/region walks below load it by hash — an unstored
            // hash failed with an internal `missing object` (#11). Storing here
            // also guarantees verify's non-storing recompute of this signature
            // resolves.
            let concrete = self.put_substituted_type(expected, &no_regions, &type_args)?;
            if !self.type_assignable_for_call_in_root(
                root,
                &arg_types[idx],
                &concrete,
                callee_regions,
            )? {
                bail!(
                    "call arg {idx} for {name} expected {}, got {}",
                    self.type_name(&concrete)?,
                    self.type_name(&arg_types[idx])?
                );
            }
            self.infer_call_region_substitutions(
                root,
                &arg_types[idx],
                &concrete,
                callee_regions,
                &mut region_substitutions,
            )?;
        }
        // 5. The call's concrete result type (type args + any inferred
        //    regions), with every nested generic instance it introduces
        //    materialized so the caller's layout/lowering can load them.
        let return_type =
            self.put_substituted_type(return_type, &region_substitutions, &type_args)?;
        self.materialize_type_instances(root, &return_type)?;
        // 6. The typed call records the generic symbol plus the inferred type
        //    arguments. (`type_args` is absent on a non-generic call, so
        //    existing call payloads — and their hashes — are unchanged.)
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "call",
                "symbol": symbol,
                "args": typed_args,
                "type": return_type,
                "type_args": type_args,
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": return_type }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash: return_type,
        })
    }

    /// Materialize every concrete generic-function instantiation reachable in a
    /// typed body as a derived root symbol (R11) — the monomorphization pass at
    /// the lowering seam. Each `(generic, type_args)` instance becomes an
    /// ordinary concrete function (substituted signature + substituted body) so
    /// reachability, lowering, linking, and bundling treat it like any other
    /// function; the instances are derived deterministically from the body, so
    /// import→export→import reproduces them and they are never projected (they
    /// have no name binding). A non-generic program adds none, so its root is
    /// unchanged. Called after a body is type-checked, before the root is
    /// stored.
    pub(crate) fn monomorphize_into_root(
        &mut self,
        root: &mut ProgramRootPayload,
        body: &str,
    ) -> Result<()> {
        let mut seeds = Vec::new();
        self.collect_concrete_generic_calls(body, &mut seeds)?;
        // Each worklist entry carries its instantiation-chain depth: 0 for calls
        // written in the source body, +1 for calls discovered inside an instance's
        // body. The `done` set terminates ordinary (mutual) generic recursion —
        // `f<T>` calling `f<T>` revisits the same instance symbol — but NOT
        // polymorphic recursion (`f<T>` calling `f<box<T>>`), where every link
        // mints a fresh symbol and the worklist would grow forever (#7). The
        // depth cap rejects those chains fail-closed; breadth (many distinct
        // shallow instantiations) stays unlimited.
        let mut worklist: Vec<(String, Vec<String>, usize)> = seeds
            .into_iter()
            .map(|(generic, type_args)| (generic, type_args, 0))
            .collect();
        let mut done = std::collections::BTreeSet::new();
        let mut next = 0;
        while next < worklist.len() {
            let (generic, type_args, depth) = worklist[next].clone();
            next += 1;
            if depth > GENERIC_INSTANTIATION_DEPTH_LIMIT {
                let shown = self
                    .symbol_display(root, &generic)
                    .unwrap_or_else(|_| generic.clone());
                bail!(
                    "generic function instantiation exceeds the depth limit ({GENERIC_INSTANTIATION_DEPTH_LIMIT}) at {shown}: a recursive generic whose type arguments grow each call (polymorphic recursion) does not converge"
                );
            }
            let instance = monomorphic_instance_symbol(&generic, &type_args);
            if !done.insert(instance.clone()) {
                continue;
            }
            if root.symbols.iter().any(|entry| entry.symbol == instance) {
                continue;
            }
            let (signature, definition, names) =
                self.build_function_instance(root, &generic, &type_args)?;
            root.symbols.push(crate::model::RootSymbolPayload {
                symbol: instance.clone(),
                definition: definition.clone(),
                signature,
            });
            root.param_names.push(crate::model::ParamNames {
                symbol: instance,
                names,
            });
            // The instance body's own generic calls (a generic function calling
            // another generic function) are now concrete — instantiate them too.
            let instance_body = self.function_body_hash(&definition)?;
            let mut nested = Vec::new();
            self.collect_concrete_generic_calls(&instance_body, &mut nested)?;
            worklist.extend(
                nested
                    .into_iter()
                    .map(|(generic, type_args)| (generic, type_args, depth + 1)),
            );
        }
        Ok(())
    }

    /// Verify every monomorphic generic-function instance in a root (R11),
    /// returning one error string per inconsistency. An instance's symbol is its
    /// descriptor's content hash (so `verify_objects` already proves the symbol
    /// matches its `generic` + `type_args`); this re-runs the import-side
    /// instantiation (`build_function_instance`) at the recorded arguments and
    /// rejects an instance whose stored signature OR body definition does not
    /// derive from its generic (H7 — the signature-only check let a tampered
    /// body that still typechecked pose as derived), plus the generic missing
    /// or non-generic, or the argument count not matching the generic's arity.
    /// Re-running stores only content-addressed objects the importer would have
    /// stored — byte-identical no-ops on an intact database.
    pub(crate) fn verify_generic_instances_in_root(
        &mut self,
        root: &ProgramRootPayload,
    ) -> Result<Vec<String>> {
        let mut errors = Vec::new();
        for entry in &root.symbols {
            if self.get_kind(&entry.symbol).ok().as_deref() != Some(MONOMORPHIC_INSTANCE_KIND) {
                continue;
            }
            let descriptor = self.get_payload(&entry.symbol)?;
            let Some(generic) = descriptor.get("generic").and_then(JsonValue::as_str) else {
                errors.push(format!(
                    "bad_generic_instance: instance {} descriptor missing generic",
                    entry.symbol
                ));
                continue;
            };
            let type_args = call_type_args(&descriptor)?;
            let Some(template) = self.root_symbol(root, generic) else {
                errors.push(format!(
                    "bad_generic_instance: instance {} names generic {generic} missing from root",
                    entry.symbol
                ));
                continue;
            };
            let type_params = self.signature_type_params(&template.signature)?;
            if type_params.is_empty() {
                errors.push(format!(
                    "bad_generic_instance: instance {} names non-generic function {generic}",
                    entry.symbol
                ));
                continue;
            }
            if type_params.len() != type_args.len() {
                errors.push(format!(
                    "bad_generic_instance: instance {} provides {} type args for generic {generic} expecting {}",
                    entry.symbol,
                    type_args.len(),
                    type_params.len()
                ));
                continue;
            }
            let generic = generic.to_string();
            let (expected_signature, expected_definition) =
                match self.build_function_instance(root, &generic, &type_args) {
                    Ok((signature, definition, _names)) => (signature, definition),
                    Err(err) => {
                        errors.push(format!(
                            "bad_generic_instance: instance {} does not rebuild from generic {generic}: {err:#}",
                            entry.symbol
                        ));
                        continue;
                    }
                };
            if entry.signature != expected_signature {
                errors.push(format!(
                    "bad_generic_instance: instance {} signature does not derive from generic {generic} at its type arguments",
                    entry.symbol
                ));
            }
            if entry.definition != expected_definition {
                errors.push(format!(
                    "bad_generic_instance: instance {} body does not derive from generic {generic} at its type arguments",
                    entry.symbol
                ));
            }
        }
        Ok(errors)
    }

    /// Build one monomorphic instance of a generic function (R11): substitute
    /// the concrete `type_args` into the template's signature and typed body,
    /// store both, and return the instance's signature, definition, and
    /// parameter names. The instance carries no type parameters — it is a
    /// concrete function keyed by the derived instance symbol.
    fn build_function_instance(
        &mut self,
        root: &ProgramRootPayload,
        generic_symbol: &str,
        type_args: &[String],
    ) -> Result<(String, String, Vec<String>)> {
        let template = self
            .root_symbol(root, generic_symbol)
            .cloned()
            .ok_or_else(|| {
                anyhow!("generic function {generic_symbol} missing for instantiation")
            })?;
        let type_params = self.signature_type_params(&template.signature)?;
        if type_params.len() != type_args.len() {
            bail!(
                "generic function {generic_symbol} expects {} type args, got {}",
                type_params.len(),
                type_args.len()
            );
        }
        let (param_types, return_type) = self.signature_parts(&template.signature)?;
        let concrete_params = param_types
            .iter()
            .map(|param| self.substitute_body_type(root, param, type_args))
            .collect::<Result<Vec<_>>>()?;
        let concrete_return = self.substitute_body_type(root, &return_type, type_args)?;
        let effects = self.signature_effects(&template.signature)?;
        let region_params = self.signature_region_params(&template.signature)?;
        // The instance signature has no type parameters: it is concrete.
        let signature = self.put_signature_with_effects_and_regions(
            &concrete_params,
            &concrete_return,
            &effects,
            &region_params,
        )?;
        let template_body = self.function_body_hash(&template.definition)?;
        let instance_body = self.substitute_typed_expr(root, &template_body, type_args)?;
        // Store the instance descriptor so its content hash — the instance's
        // symbol — is a real object (a root symbol references `objects`). The
        // hash matches the pure `monomorphic_instance_symbol`.
        let instance_symbol = self.put_object(
            MONOMORPHIC_INSTANCE_KIND,
            &monomorphic_instance_descriptor(generic_symbol, type_args),
        )?;
        let definition = self.put_function_def(&instance_symbol, &signature, &instance_body)?;
        let names = crate::model::param_names(root, generic_symbol);
        Ok((signature, definition, names))
    }

    /// Substitute a generic function's type arguments into a type from its body
    /// (R11) and materialize any nested generic instance the substitution
    /// produces, so the monomorphized body's types are concrete and stored for
    /// layout/lowering.
    fn substitute_body_type(
        &mut self,
        root: &ProgramRootPayload,
        type_hash: &str,
        type_args: &[String],
    ) -> Result<String> {
        let no_regions = BTreeMap::new();
        let concrete = self.put_substituted_type(type_hash, &no_regions, type_args)?;
        self.materialize_type_instances(root, &concrete)?;
        Ok(concrete)
    }

    /// Collect every concrete generic-function call reachable in a typed body —
    /// `(generic_symbol, type_args)` pairs whose type arguments are fully
    /// concrete (R11). A call inside a generic template body has `TypeParam`
    /// arguments and is skipped (the template is not lowered; its instances are
    /// materialized when the template is itself instantiated). The traversal is
    /// structural and deterministic, so the materialization order — and the
    /// resulting root — reproduce on re-import.
    fn collect_concrete_generic_calls(
        &self,
        expr_hash: &str,
        out: &mut Vec<(String, Vec<String>)>,
    ) -> Result<()> {
        let payload = self.get_payload(expr_hash)?;
        if payload.get("expr_kind").and_then(JsonValue::as_str) == Some("call") {
            let type_args = call_type_args(&payload)?;
            if !type_args.is_empty()
                && type_args
                    .iter()
                    .all(|arg| self.type_is_concrete(arg).unwrap_or(false))
            {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                out.push((symbol.to_string(), type_args));
            }
        }
        for child in self.child_expr_hashes(&payload)? {
            self.collect_concrete_generic_calls(&child, out)?;
        }
        Ok(())
    }

    /// Whether `type_hash` contains no `TypeParam` (R11) — i.e. it is a concrete
    /// type at which a generic function may be monomorphized.
    pub(crate) fn type_is_concrete(&self, type_hash: &str) -> Result<bool> {
        Ok(match self.type_spec(type_hash)? {
            TypeSpec::TypeParam { .. } => false,
            TypeSpec::Builtin(_) | TypeSpec::String => true,
            TypeSpec::Named { type_args, .. } => {
                let mut concrete = true;
                for arg in type_args {
                    concrete &= self.type_is_concrete(&arg)?;
                }
                concrete
            }
            TypeSpec::Reference { referent: inner, .. }
            | TypeSpec::RawPointer { pointee: inner, .. }
            | TypeSpec::Box { element: inner }
            | TypeSpec::Vec { element: inner }
            | TypeSpec::Slice { element: inner, .. }
            | TypeSpec::FixedArray { element: inner, .. } => self.type_is_concrete(&inner)?,
            TypeSpec::Record(members) | TypeSpec::Enum(members) => {
                let mut concrete = true;
                for member in members {
                    concrete &= self.type_is_concrete(&member.type_hash)?;
                }
                concrete
            }
        })
    }

    /// The child *expression* hashes of a typed expression, in source order
    /// (R11). Centralizes the per-kind child structure shared by the
    /// monomorphization traversals (mirrors `collect_expr_deps`); the
    /// substitution walker overrides each child in place.
    pub(crate) fn child_expr_hashes(&self, payload: &JsonValue) -> Result<Vec<String>> {
        let kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind"))?;
        let field = |key: &str| -> Result<String> {
            payload
                .get(key)
                .and_then(JsonValue::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("{kind} missing {key}"))
        };
        let mut children = Vec::new();
        match kind {
            "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" | "param_ref"
            | "local_ref" => {}
            "call" => {
                for arg in payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                {
                    children.push(
                        arg.as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?
                            .to_string(),
                    );
                }
            }
            "record_literal" => {
                for member in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                {
                    children.push(
                        member
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing value"))?
                            .to_string(),
                    );
                }
            }
            "array_literal" => {
                for element in payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                {
                    children.push(
                        element
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array element missing value"))?
                            .to_string(),
                    );
                }
            }
            "case" => {
                children.push(field("expr")?);
                for arm in payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?
                {
                    if let Some(guard) = arm.get("guard").and_then(JsonValue::as_str) {
                        children.push(guard.to_string());
                    }
                    children.push(
                        arm.get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?
                            .to_string(),
                    );
                }
            }
            other => {
                for key in plain_child_expr_keys(other)? {
                    children.push(field(key)?);
                }
            }
        }
        Ok(children)
    }

    /// Rewrite a typed expression with a generic function's type arguments
    /// substituted into every type it carries (R11), recursing into all child
    /// expressions, and return the new expression's hash. Used to produce a
    /// monomorphic instance's concrete body from the generic template's body:
    /// every `type`/`*_type` field and `type_args` list is substituted (and any
    /// nested generic instance materialized), so the result is an ordinary
    /// concrete typed expression the existing layout/lowering/verify accept.
    fn substitute_typed_expr(
        &mut self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_args: &[String],
    ) -> Result<String> {
        let payload = self.get_payload(expr_hash)?;
        let object = payload
            .as_object()
            .ok_or_else(|| anyhow!("expression payload must be an object"))?;
        let kind = object
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind"))?
            .to_string();
        // Substitute every type-bearing top-level field. Type hashes live under
        // `type`, every `*_type` key, and the `type_args` list; all other
        // scalar fields (names, operators, variants, counts) are copied
        // verbatim, and child expressions are overwritten below.
        let mut out = serde_json::Map::new();
        for (key, value) in object {
            if key == "type_args" {
                let mut args = Vec::new();
                for arg in value
                    .as_array()
                    .ok_or_else(|| anyhow!("type_args must be an array"))?
                {
                    let arg = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("type arg must be a hash"))?;
                    args.push(self.substitute_body_type(root, arg, type_args)?);
                }
                out.insert(key.clone(), json!(args));
            } else if key == "type" || key.ends_with("_type") {
                let hash = value
                    .as_str()
                    .ok_or_else(|| anyhow!("type field {key} must be a hash"))?;
                out.insert(
                    key.clone(),
                    json!(self.substitute_body_type(root, hash, type_args)?),
                );
            } else {
                out.insert(key.clone(), value.clone());
            }
        }
        // Recurse into child expressions, rebuilding the kinds with structured
        // children (call args, record/array members, case arms) and overwriting
        // the plain single-/multi-child keys for the rest.
        match kind.as_str() {
            "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" | "param_ref"
            | "local_ref" => {}
            "call" => {
                let mut args = Vec::new();
                for arg in object
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                {
                    let arg = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    args.push(self.substitute_typed_expr(root, arg, type_args)?);
                }
                out.insert("args".to_string(), json!(args));
            }
            "record_literal" => {
                let mut fields = Vec::new();
                for member in object
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                {
                    let name = member.get("name").cloned().unwrap_or(JsonValue::Null);
                    let value = member
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing value"))?;
                    let field_type = member
                        .get("type")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing type"))?;
                    fields.push(json!({
                        "name": name,
                        "value": self.substitute_typed_expr(root, value, type_args)?,
                        "type": self.substitute_body_type(root, field_type, type_args)?,
                    }));
                }
                out.insert("fields".to_string(), json!(fields));
            }
            "array_literal" => {
                let mut elements = Vec::new();
                for element in object
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                {
                    let value = element
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing value"))?;
                    let element_type = element
                        .get("type")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing type"))?;
                    elements.push(json!({
                        "value": self.substitute_typed_expr(root, value, type_args)?,
                        "type": self.substitute_body_type(root, element_type, type_args)?,
                    }));
                }
                out.insert("elements".to_string(), json!(elements));
            }
            "case" => {
                let scrutinee = object
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                out.insert(
                    "expr".to_string(),
                    json!(self.substitute_typed_expr(root, scrutinee, type_args)?),
                );
                let mut arms = Vec::new();
                for arm in object
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?
                {
                    // Arm patterns (`variant`/`binding_name`/`payload_pattern`/
                    // `default`) carry no type hashes — the binding types are
                    // re-derived from the (substituted) scrutinee type at
                    // lowering — so copy them and rewrite only the guard and body.
                    let mut new_arm = arm
                        .as_object()
                        .ok_or_else(|| anyhow!("case arm must be an object"))?
                        .clone();
                    if let Some(guard) = arm.get("guard").and_then(JsonValue::as_str) {
                        new_arm.insert(
                            "guard".to_string(),
                            json!(self.substitute_typed_expr(root, guard, type_args)?),
                        );
                    }
                    let body = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    new_arm.insert(
                        "body".to_string(),
                        json!(self.substitute_typed_expr(root, body, type_args)?),
                    );
                    arms.push(JsonValue::Object(new_arm));
                }
                out.insert("arms".to_string(), json!(arms));
            }
            other => {
                for key in plain_child_expr_keys(other)? {
                    let child = object
                        .get(*key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("{other} missing {key}"))?;
                    out.insert(
                        key.to_string(),
                        json!(self.substitute_typed_expr(root, child, type_args)?),
                    );
                }
            }
        }
        self.put_object("Expression", &JsonValue::Object(out))
    }

    pub(crate) fn enum_variant_type_in_root(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        variant: &str,
    ) -> Result<String> {
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Enum(variants) => variants
                .into_iter()
                .find(|candidate| candidate.name == variant)
                .map(|candidate| candidate.type_hash)
                .ok_or_else(|| anyhow!("enum has no variant {variant}")),
            other => bail!(
                "enum variant construction requires enum type, got {}",
                other.to_source(self)?
            ),
        }
    }

    pub(crate) fn field_access_type_in_root(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        field: &str,
    ) -> Result<String> {
        match self.type_spec(type_hash)? {
            TypeSpec::Reference { referent, .. } => {
                self.record_field_type_in_root(root, &referent, field)
            }
            TypeSpec::Box { element } => self.record_field_type_in_root(root, &element, field),
            _ => self.record_field_type_in_root(root, type_hash, field),
        }
    }

    pub(crate) fn type_assignable_in_root(
        &self,
        root: &ProgramRootPayload,
        actual: &str,
        expected: &str,
    ) -> Result<bool> {
        if actual == expected {
            return Ok(true);
        }
        if let Some(assignable) =
            named_actual_type_assignable(&self.type_spec(actual)?, &self.type_spec(expected)?)
        {
            return Ok(assignable);
        }
        match (
            self.type_spec_in_root(root, actual)?,
            self.type_spec_in_root(root, expected)?,
        ) {
            (
                TypeSpec::Reference {
                    region: actual_region,
                    mutable: actual_mutable,
                    referent: actual_referent,
                },
                TypeSpec::Reference {
                    region: expected_region,
                    mutable: expected_mutable,
                    referent: expected_referent,
                },
            ) => {
                if actual_region != expected_region || actual_mutable != expected_mutable {
                    return Ok(false);
                }
                self.type_assignable_in_root(root, &actual_referent, &expected_referent)
            }
            (
                TypeSpec::Slice {
                    region: actual_region,
                    mutable: actual_mutable,
                    element: actual_element,
                },
                TypeSpec::Slice {
                    region: expected_region,
                    mutable: expected_mutable,
                    element: expected_element,
                },
            ) => {
                if actual_region != expected_region || actual_mutable != expected_mutable {
                    return Ok(false);
                }
                self.type_assignable_in_root(root, &actual_element, &expected_element)
            }
            (
                TypeSpec::Box {
                    element: actual_element,
                },
                TypeSpec::Box {
                    element: expected_element,
                },
            ) => self.type_assignable_in_root(root, &actual_element, &expected_element),
            (
                TypeSpec::Vec {
                    element: actual_element,
                },
                TypeSpec::Vec {
                    element: expected_element,
                },
            ) => self.type_assignable_in_root(root, &actual_element, &expected_element),
            (
                TypeSpec::FixedArray {
                    element: actual_element,
                    len: actual_len,
                },
                TypeSpec::FixedArray {
                    element: expected_element,
                    len: expected_len,
                },
            ) => {
                if actual_len != expected_len {
                    return Ok(false);
                }
                self.type_assignable_in_root(root, &actual_element, &expected_element)
            }
            (TypeSpec::Record(actual_fields), TypeSpec::Record(expected_fields))
            | (TypeSpec::Enum(actual_fields), TypeSpec::Enum(expected_fields)) => {
                if actual_fields.len() != expected_fields.len() {
                    return Ok(false);
                }
                for expected_field in expected_fields {
                    let Some(actual_field) = actual_fields
                        .iter()
                        .find(|field| field.name == expected_field.name)
                    else {
                        return Ok(false);
                    };
                    if !self.type_assignable_in_root(
                        root,
                        &actual_field.type_hash,
                        &expected_field.type_hash,
                    )? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(crate) fn type_assignable_for_call_in_root(
        &self,
        root: &ProgramRootPayload,
        actual: &str,
        expected: &str,
        callee_regions: &BTreeSet<String>,
    ) -> Result<bool> {
        if actual == expected {
            return Ok(true);
        }
        if let Some(assignable) = named_actual_type_assignable_for_call(
            &self.type_spec(actual)?,
            &self.type_spec(expected)?,
            callee_regions,
        ) {
            return Ok(assignable);
        }
        match (
            self.type_spec_in_root(root, actual)?,
            self.type_spec_in_root(root, expected)?,
        ) {
            (
                TypeSpec::Reference {
                    region: actual_region,
                    mutable: actual_mutable,
                    referent: actual_referent,
                },
                TypeSpec::Reference {
                    region: expected_region,
                    mutable: expected_mutable,
                    referent: expected_referent,
                },
            ) => {
                if actual_mutable != expected_mutable {
                    return Ok(false);
                }
                if actual_region != expected_region && !callee_regions.contains(&expected_region) {
                    return Ok(false);
                }
                self.type_assignable_for_call_in_root(
                    root,
                    &actual_referent,
                    &expected_referent,
                    callee_regions,
                )
            }
            (
                TypeSpec::Slice {
                    region: actual_region,
                    mutable: actual_mutable,
                    element: actual_element,
                },
                TypeSpec::Slice {
                    region: expected_region,
                    mutable: expected_mutable,
                    element: expected_element,
                },
            ) => {
                if actual_mutable != expected_mutable {
                    return Ok(false);
                }
                if actual_region != expected_region && !callee_regions.contains(&expected_region) {
                    return Ok(false);
                }
                self.type_assignable_for_call_in_root(
                    root,
                    &actual_element,
                    &expected_element,
                    callee_regions,
                )
            }
            (
                TypeSpec::Box {
                    element: actual_element,
                },
                TypeSpec::Box {
                    element: expected_element,
                },
            ) => self.type_assignable_for_call_in_root(
                root,
                &actual_element,
                &expected_element,
                callee_regions,
            ),
            (
                TypeSpec::Vec {
                    element: actual_element,
                },
                TypeSpec::Vec {
                    element: expected_element,
                },
            ) => self.type_assignable_for_call_in_root(
                root,
                &actual_element,
                &expected_element,
                callee_regions,
            ),
            (
                TypeSpec::FixedArray {
                    element: actual_element,
                    len: actual_len,
                },
                TypeSpec::FixedArray {
                    element: expected_element,
                    len: expected_len,
                },
            ) => {
                if actual_len != expected_len {
                    return Ok(false);
                }
                self.type_assignable_for_call_in_root(
                    root,
                    &actual_element,
                    &expected_element,
                    callee_regions,
                )
            }
            (TypeSpec::Record(actual_fields), TypeSpec::Record(expected_fields))
            | (TypeSpec::Enum(actual_fields), TypeSpec::Enum(expected_fields)) => {
                if actual_fields.len() != expected_fields.len() {
                    return Ok(false);
                }
                for expected_field in expected_fields {
                    let Some(actual_field) = actual_fields
                        .iter()
                        .find(|field| field.name == expected_field.name)
                    else {
                        return Ok(false);
                    };
                    if !self.type_assignable_for_call_in_root(
                        root,
                        &actual_field.type_hash,
                        &expected_field.type_hash,
                        callee_regions,
                    )? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn infer_call_region_substitutions(
        &self,
        root: &ProgramRootPayload,
        actual: &str,
        expected: &str,
        callee_regions: &BTreeSet<String>,
        substitutions: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        if actual == expected {
            return Ok(());
        }
        if self.infer_named_type_region_substitutions(
            actual,
            expected,
            callee_regions,
            substitutions,
        )? {
            return Ok(());
        }
        match (
            self.type_spec_in_root(root, actual)?,
            self.type_spec_in_root(root, expected)?,
        ) {
            (
                TypeSpec::Reference {
                    region: actual_region,
                    mutable: actual_mutable,
                    referent: actual_referent,
                },
                TypeSpec::Reference {
                    region: expected_region,
                    mutable: expected_mutable,
                    referent: expected_referent,
                },
            ) => {
                if actual_mutable != expected_mutable {
                    return Ok(());
                }
                record_call_region_substitution(
                    expected_region,
                    actual_region,
                    callee_regions,
                    substitutions,
                )?;
                self.infer_call_region_substitutions(
                    root,
                    &actual_referent,
                    &expected_referent,
                    callee_regions,
                    substitutions,
                )
            }
            (
                TypeSpec::Named {
                    type_symbol: actual_symbol,
                    region_args: actual_args,
                    type_args: actual_type_args,
                },
                TypeSpec::Named {
                    type_symbol: expected_symbol,
                    region_args: expected_args,
                    type_args: expected_type_args,
                },
            ) => {
                if actual_symbol != expected_symbol
                    || actual_args.len() != expected_args.len()
                    || actual_type_args.len() != expected_type_args.len()
                {
                    return Ok(());
                }
                for (actual_region, expected_region) in actual_args.into_iter().zip(expected_args) {
                    record_call_region_substitution(
                        expected_region,
                        actual_region,
                        callee_regions,
                        substitutions,
                    )?;
                }
                // Recurse into the generic instance's type arguments so a region
                // nested inside one (e.g. `Holder<&'r i64>`) is inferred too (R11).
                for (actual_arg, expected_arg) in
                    actual_type_args.into_iter().zip(expected_type_args)
                {
                    self.infer_call_region_substitutions(
                        root,
                        &actual_arg,
                        &expected_arg,
                        callee_regions,
                        substitutions,
                    )?;
                }
                Ok(())
            }
            (
                TypeSpec::RawPointer {
                    pointee: actual, ..
                },
                TypeSpec::RawPointer {
                    pointee: expected, ..
                },
            ) => self.infer_call_region_substitutions(
                root,
                &actual,
                &expected,
                callee_regions,
                substitutions,
            ),
            (TypeSpec::Box { element: actual }, TypeSpec::Box { element: expected }) => self
                .infer_call_region_substitutions(
                    root,
                    &actual,
                    &expected,
                    callee_regions,
                    substitutions,
                ),
            (TypeSpec::Vec { element: actual }, TypeSpec::Vec { element: expected }) => self
                .infer_call_region_substitutions(
                    root,
                    &actual,
                    &expected,
                    callee_regions,
                    substitutions,
                ),
            (
                TypeSpec::Slice {
                    region: actual_region,
                    mutable: actual_mutable,
                    element: actual_element,
                },
                TypeSpec::Slice {
                    region: expected_region,
                    mutable: expected_mutable,
                    element: expected_element,
                },
            ) => {
                if actual_mutable != expected_mutable {
                    return Ok(());
                }
                record_call_region_substitution(
                    expected_region,
                    actual_region,
                    callee_regions,
                    substitutions,
                )?;
                self.infer_call_region_substitutions(
                    root,
                    &actual_element,
                    &expected_element,
                    callee_regions,
                    substitutions,
                )
            }
            (
                TypeSpec::FixedArray {
                    element: actual,
                    len: actual_len,
                },
                TypeSpec::FixedArray {
                    element: expected,
                    len: expected_len,
                },
            ) => {
                if actual_len != expected_len {
                    return Ok(());
                }
                self.infer_call_region_substitutions(
                    root,
                    &actual,
                    &expected,
                    callee_regions,
                    substitutions,
                )
            }
            (TypeSpec::Record(actual_fields), TypeSpec::Record(expected_fields))
            | (TypeSpec::Enum(actual_fields), TypeSpec::Enum(expected_fields)) => {
                for expected_field in expected_fields {
                    if let Some(actual_field) = actual_fields
                        .iter()
                        .find(|field| field.name == expected_field.name)
                    {
                        self.infer_call_region_substitutions(
                            root,
                            &actual_field.type_hash,
                            &expected_field.type_hash,
                            callee_regions,
                            substitutions,
                        )?;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn infer_named_type_region_substitutions(
        &self,
        actual: &str,
        expected: &str,
        callee_regions: &BTreeSet<String>,
        substitutions: &mut BTreeMap<String, String>,
    ) -> Result<bool> {
        let (
            TypeSpec::Named {
                type_symbol: actual_symbol,
                region_args: actual_args,
                ..
            },
            TypeSpec::Named {
                type_symbol: expected_symbol,
                region_args: expected_args,
                ..
            },
        ) = (self.type_spec(actual)?, self.type_spec(expected)?)
        else {
            return Ok(false);
        };
        if actual_symbol != expected_symbol || actual_args.len() != expected_args.len() {
            return Ok(true);
        }
        for (actual_region, expected_region) in actual_args.into_iter().zip(expected_args) {
            record_call_region_substitution(
                expected_region,
                actual_region,
                callee_regions,
                substitutions,
            )?;
        }
        Ok(true)
    }

    pub(crate) fn infer_call_region_substitutions_for_types(
        &self,
        root: &ProgramRootPayload,
        actual: &str,
        expected: &str,
        callee_regions: &BTreeSet<String>,
        substitutions: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        self.infer_call_region_substitutions(root, actual, expected, callee_regions, substitutions)
    }

    pub(crate) fn substitute_type_regions_hash_for_verify(
        &self,
        type_hash: &str,
        region_substitutions: &BTreeMap<String, String>,
    ) -> Result<String> {
        self.substitute_type_regions_hash(type_hash, region_substitutions)
    }

    fn value_class_in_root(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<ValueClass> {
        // Generic functions (R11): a type mentioning a `TypeParam` (a bare `T`,
        // or `Option<T>`, `Pair<T>`, ...) has no concrete layout, so it gets the
        // most conservative parametric classification — move-only and needs-drop
        // (the generic body may not assume `T` is copyable, which is exactly
        // constraint-free parametricity). The concrete copy/drop behaviour is
        // recovered per instantiation, when the parameters are substituted away
        // and the monomorphic body is checked with real types.
        if !self.type_is_concrete(type_hash)? {
            return Ok(ValueClass {
                copy_kind: ValueCopyKind::MoveOnly,
                drop_kind: ValueDropKind::NeedsDrop,
                contains_reference: false,
                contains_mut_reference: false,
                contains_box: false,
            });
        }
        let layout = self.compute_type_layout(root, type_hash, DEFAULT_NATIVE_TARGET)?;
        let copy_kind = match layout.metadata.get("copy_kind").and_then(JsonValue::as_str) {
            Some("copy") => ValueCopyKind::Copy,
            Some("move_only") => ValueCopyKind::MoveOnly,
            Some(other) => bail!("unknown copy_kind {other} for type {type_hash}"),
            None => bail!("type layout missing copy_kind for {type_hash}"),
        };
        let drop_kind = match layout.metadata.get("drop_kind").and_then(JsonValue::as_str) {
            Some("trivial") => ValueDropKind::Trivial,
            Some("needs_drop") => ValueDropKind::NeedsDrop,
            Some(other) => bail!("unknown drop_kind {other} for type {type_hash}"),
            None => bail!("type layout missing drop_kind for {type_hash}"),
        };
        Ok(ValueClass {
            copy_kind,
            drop_kind,
            contains_reference: layout
                .metadata
                .get("contains_reference")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| anyhow!("type layout missing contains_reference for {type_hash}"))?,
            contains_mut_reference: layout
                .metadata
                .get("contains_mut_reference")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| {
                    anyhow!("type layout missing contains_mut_reference for {type_hash}")
                })?,
            contains_box: layout
                .metadata
                .get("contains_box")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| anyhow!("type layout missing contains_box for {type_hash}"))?,
        })
    }

    #[allow(dead_code)]
    fn put_type_spec(&mut self, spec: &ParsedTypeSpec) -> Result<String> {
        match spec {
            ParsedTypeSpec::Builtin(kind) => Ok(type_hash_for(kind)),
            ParsedTypeSpec::Named { name, .. } => {
                bail!("named type {name} requires root-aware resolution")
            }
            ParsedTypeSpec::TypeParam { index } => {
                self.put_structural_type(TypeSpec::TypeParam { index: *index })
            }
            ParsedTypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                bail!(
                    "reference region '{region} requires root-aware resolution before resolving {referent:?} as mutable={mutable}"
                )
            }
            ParsedTypeSpec::RawPointer { mutable, pointee } => {
                let pointee = self.put_type_spec(pointee)?;
                self.put_structural_type(TypeSpec::RawPointer {
                    mutable: *mutable,
                    pointee,
                })
            }
            ParsedTypeSpec::Box { element } => {
                let element = self.put_type_spec(element)?;
                self.put_structural_type(TypeSpec::Box { element })
            }
            ParsedTypeSpec::Vec { element } => {
                let element = self.put_type_spec(element)?;
                self.put_structural_type(TypeSpec::Vec { element })
            }
            ParsedTypeSpec::String => self.put_structural_type(TypeSpec::String),
            ParsedTypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                bail!(
                    "slice region '{region} requires root-aware resolution before resolving {element:?} as mutable={mutable}"
                )
            }
            ParsedTypeSpec::FixedArray { element, len } => {
                let element = self.put_type_spec(element)?;
                self.put_structural_type(TypeSpec::FixedArray { element, len: *len })
            }
            ParsedTypeSpec::Record(fields) => {
                let fields = fields
                    .iter()
                    .map(|field| {
                        Ok(TypeFieldSpec {
                            name: field.name.clone(),
                            type_hash: self.put_type_spec(&field.ty)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.put_structural_type(TypeSpec::Record(fields))
            }
            ParsedTypeSpec::Enum(variants) => {
                let variants = variants
                    .iter()
                    .map(|variant| {
                        Ok(TypeFieldSpec {
                            name: variant.name.clone(),
                            type_hash: self.put_type_spec(&variant.ty)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.put_structural_type(TypeSpec::Enum(variants))
            }
        }
    }

    fn put_type_spec_in_root(
        &mut self,
        current_module: &str,
        root: &ProgramRootPayload,
        spec: &ParsedTypeSpec,
        region_scope: &BTreeMap<String, String>,
    ) -> Result<String> {
        match spec {
            ParsedTypeSpec::Builtin(kind) => Ok(type_hash_for(kind)),
            ParsedTypeSpec::Named {
                name,
                region_args,
                type_args,
            } => {
                let type_symbol = resolve_named_type_in_root(root, current_module, name)
                    .ok_or_else(|| anyhow!("unknown type {name}"))?;
                let entry = self
                    .root_type(root, &type_symbol)
                    .ok_or_else(|| anyhow!("type {name} missing root definition"))?;
                let definition = self.type_definition(&entry.type_def)?;
                if definition.region_params().len() != region_args.len() {
                    bail!(
                        "type {name} expects {} region args, got {}",
                        definition.region_params().len(),
                        region_args.len()
                    );
                }
                if definition.type_params().len() != type_args.len() {
                    bail!(
                        "type {name} expects {} type args, got {}",
                        definition.type_params().len(),
                        type_args.len()
                    );
                }
                let region_args = resolve_region_args(region_args, region_scope)?;
                let type_args = type_args
                    .iter()
                    .map(|arg| {
                        self.put_type_spec_in_root(current_module, root, arg, region_scope)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let type_hash = self.put_structural_type(TypeSpec::Named {
                    type_symbol: type_symbol.clone(),
                    region_args: region_args.clone(),
                    type_args: type_args.clone(),
                })?;
                self.materialize_named_type_expansion(
                    root,
                    &type_symbol,
                    &region_args,
                    &type_args,
                )?;
                Ok(type_hash)
            }
            ParsedTypeSpec::TypeParam { index } => {
                self.put_structural_type(TypeSpec::TypeParam { index: *index })
            }
            ParsedTypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let referent =
                    self.put_type_spec_in_root(current_module, root, referent, region_scope)?;
                let region = resolve_region_arg(region, region_scope)?;
                self.put_structural_type(TypeSpec::Reference {
                    region,
                    mutable: *mutable,
                    referent,
                })
            }
            ParsedTypeSpec::RawPointer { mutable, pointee } => {
                let pointee =
                    self.put_type_spec_in_root(current_module, root, pointee, region_scope)?;
                self.put_structural_type(TypeSpec::RawPointer {
                    mutable: *mutable,
                    pointee,
                })
            }
            ParsedTypeSpec::Box { element } => {
                let element =
                    self.put_type_spec_in_root(current_module, root, element, region_scope)?;
                self.put_structural_type(TypeSpec::Box { element })
            }
            ParsedTypeSpec::Vec { element } => {
                let element =
                    self.put_type_spec_in_root(current_module, root, element, region_scope)?;
                self.put_structural_type(TypeSpec::Vec { element })
            }
            ParsedTypeSpec::String => self.put_structural_type(TypeSpec::String),
            ParsedTypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let element =
                    self.put_type_spec_in_root(current_module, root, element, region_scope)?;
                let region = resolve_region_arg(region, region_scope)?;
                self.put_structural_type(TypeSpec::Slice {
                    region,
                    mutable: *mutable,
                    element,
                })
            }
            ParsedTypeSpec::FixedArray { element, len } => {
                let element =
                    self.put_type_spec_in_root(current_module, root, element, region_scope)?;
                self.put_structural_type(TypeSpec::FixedArray { element, len: *len })
            }
            ParsedTypeSpec::Record(fields) => {
                let fields = fields
                    .iter()
                    .map(|field| {
                        Ok(TypeFieldSpec {
                            name: field.name.clone(),
                            type_hash: self.put_type_spec_in_root(
                                current_module,
                                root,
                                &field.ty,
                                region_scope,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.put_structural_type(TypeSpec::Record(fields))
            }
            ParsedTypeSpec::Enum(variants) => {
                let variants = variants
                    .iter()
                    .map(|variant| {
                        Ok(TypeFieldSpec {
                            name: variant.name.clone(),
                            type_hash: self.put_type_spec_in_root(
                                current_module,
                                root,
                                &variant.ty,
                                region_scope,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.put_structural_type(TypeSpec::Enum(variants))
            }
        }
    }

    fn type_hash_for_parsed_in_root(
        &self,
        current_module: &str,
        root: &ProgramRootPayload,
        spec: &ParsedTypeSpec,
        region_scope: &BTreeMap<String, String>,
    ) -> Result<String> {
        match spec {
            ParsedTypeSpec::Builtin(kind) => Ok(type_hash_for(kind)),
            ParsedTypeSpec::Named {
                name,
                region_args,
                type_args,
            } => {
                let type_symbol = resolve_named_type_in_root(root, current_module, name)
                    .ok_or_else(|| anyhow!("unknown type {name}"))?;
                let entry = self
                    .root_type(root, &type_symbol)
                    .ok_or_else(|| anyhow!("type {name} missing root definition"))?;
                let definition = self.type_definition(&entry.type_def)?;
                if definition.region_params().len() != region_args.len() {
                    bail!(
                        "type {name} expects {} region args, got {}",
                        definition.region_params().len(),
                        region_args.len()
                    );
                }
                if definition.type_params().len() != type_args.len() {
                    bail!(
                        "type {name} expects {} type args, got {}",
                        definition.type_params().len(),
                        type_args.len()
                    );
                }
                hash_for_type_spec(&TypeSpec::Named {
                    type_symbol,
                    region_args: resolve_region_args(region_args, region_scope)?,
                    type_args: type_args
                        .iter()
                        .map(|arg| {
                            self.type_hash_for_parsed_in_root(current_module, root, arg, region_scope)
                        })
                        .collect::<Result<Vec<_>>>()?,
                })
            }
            ParsedTypeSpec::TypeParam { index } => {
                hash_for_type_spec(&TypeSpec::TypeParam { index: *index })
            }
            ParsedTypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let referent = self.type_hash_for_parsed_in_root(
                    current_module,
                    root,
                    referent,
                    region_scope,
                )?;
                hash_for_type_spec(&TypeSpec::Reference {
                    region: resolve_region_arg(region, region_scope)?,
                    mutable: *mutable,
                    referent,
                })
            }
            ParsedTypeSpec::RawPointer { mutable, pointee } => {
                let pointee =
                    self.type_hash_for_parsed_in_root(current_module, root, pointee, region_scope)?;
                hash_for_type_spec(&TypeSpec::RawPointer {
                    mutable: *mutable,
                    pointee,
                })
            }
            ParsedTypeSpec::Box { element } => {
                let element =
                    self.type_hash_for_parsed_in_root(current_module, root, element, region_scope)?;
                hash_for_type_spec(&TypeSpec::Box { element })
            }
            ParsedTypeSpec::Vec { element } => {
                let element =
                    self.type_hash_for_parsed_in_root(current_module, root, element, region_scope)?;
                hash_for_type_spec(&TypeSpec::Vec { element })
            }
            ParsedTypeSpec::String => hash_for_type_spec(&TypeSpec::String),
            ParsedTypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let element =
                    self.type_hash_for_parsed_in_root(current_module, root, element, region_scope)?;
                hash_for_type_spec(&TypeSpec::Slice {
                    region: resolve_region_arg(region, region_scope)?,
                    mutable: *mutable,
                    element,
                })
            }
            ParsedTypeSpec::FixedArray { element, len } => {
                let element =
                    self.type_hash_for_parsed_in_root(current_module, root, element, region_scope)?;
                hash_for_type_spec(&TypeSpec::FixedArray { element, len: *len })
            }
            ParsedTypeSpec::Record(fields) => {
                let fields = fields
                    .iter()
                    .map(|field| {
                        Ok(TypeFieldSpec {
                            name: field.name.clone(),
                            type_hash: self.type_hash_for_parsed_in_root(
                                current_module,
                                root,
                                &field.ty,
                                region_scope,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                hash_for_type_spec(&TypeSpec::Record(fields))
            }
            ParsedTypeSpec::Enum(variants) => {
                let variants = variants
                    .iter()
                    .map(|variant| {
                        Ok(TypeFieldSpec {
                            name: variant.name.clone(),
                            type_hash: self.type_hash_for_parsed_in_root(
                                current_module,
                                root,
                                &variant.ty,
                                region_scope,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                hash_for_type_spec(&TypeSpec::Enum(variants))
            }
        }
    }

    fn put_structural_type(&mut self, spec: TypeSpec) -> Result<String> {
        let payload = type_payload_for_spec(&spec)?;
        self.put_object("Type", &payload)
    }

    #[allow(dead_code)]
    pub(crate) fn put_signature(
        &mut self,
        param_types: &[String],
        return_type: &str,
    ) -> Result<String> {
        self.put_signature_with_effects(param_types, return_type, &[])
    }

    pub(crate) fn put_signature_with_effects(
        &mut self,
        param_types: &[String],
        return_type: &str,
        effects: &[Effect],
    ) -> Result<String> {
        self.put_signature_with_effects_and_regions(param_types, return_type, effects, &[])
    }

    pub(crate) fn put_signature_with_effects_and_regions(
        &mut self,
        param_types: &[String],
        return_type: &str,
        effects: &[Effect],
        region_params: &[RegionParamDef],
    ) -> Result<String> {
        self.put_signature_with_effects_regions_and_type_params(
            param_types,
            return_type,
            effects,
            region_params,
            &[],
        )
    }

    /// Build a function signature carrying type parameters (R11). A non-empty
    /// `type_params` names the generic function's positional parameters and
    /// makes the signature a *generic template* whose `params`/`return` use
    /// `TypeSpec::TypeParam { index }`. `type_params` is skipped when empty so a
    /// non-generic signature's payload — and therefore its content hash — is
    /// byte-identical to the pre-generics form (the whole existing corpus keeps
    /// its hashes).
    pub(crate) fn put_signature_with_effects_regions_and_type_params(
        &mut self,
        param_types: &[String],
        return_type: &str,
        effects: &[Effect],
        region_params: &[RegionParamDef],
        type_params: &[String],
    ) -> Result<String> {
        let effects = normalize_effects(effects)?;
        validate_region_params(region_params)?;
        let mut payload = serde_json::Map::new();
        if !region_params.is_empty() {
            payload.insert(
                "region_params".to_string(),
                json!(
                    region_params
                        .iter()
                        .map(|param| json!({ "region": param.region, "name": param.name }))
                        .collect::<Vec<_>>()
                ),
            );
        }
        if !type_params.is_empty() {
            payload.insert("type_params".to_string(), json!(type_params));
        }
        payload.insert("params".to_string(), json!(param_types));
        payload.insert("return".to_string(), json!(return_type));
        payload.insert("abi".to_string(), json!(ABI_TAG));
        payload.insert("effects".to_string(), json!(effect_names(&effects)));
        self.put_object("FunctionSignature", &JsonValue::Object(payload))
    }

    /// The type-parameter names of a function signature (R11), empty for a
    /// non-generic function. The length is the generic arity; the names drive
    /// the `TypeParam` scope when checking/projecting the generic template.
    pub(crate) fn signature_type_params(&self, signature_hash: &str) -> Result<Vec<String>> {
        let payload = self.get_payload(signature_hash)?;
        match payload.get("type_params") {
            None => Ok(Vec::new()),
            Some(JsonValue::Array(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("signature type param must be a string"))
                })
                .collect::<Result<Vec<_>>>(),
            Some(_) => bail!("signature type_params must be an array {signature_hash}"),
        }
    }

    pub(crate) fn signature_parts(&self, signature_hash: &str) -> Result<(Vec<String>, String)> {
        let payload = self.get_payload(signature_hash)?;
        let params = payload
            .get("params")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("signature missing params {signature_hash}"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("signature param must be hash"))
            })
            .collect::<Result<Vec<_>>>()?;
        let return_type = payload
            .get("return")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("signature missing return {signature_hash}"))?
            .to_string();
        Ok((params, return_type))
    }

    pub(crate) fn signature_effects(&self, signature_hash: &str) -> Result<Vec<Effect>> {
        let payload = self.get_payload(signature_hash)?;
        let effects = match payload.get("effects") {
            None => Vec::new(),
            Some(JsonValue::Array(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| anyhow!("signature effect must be string"))
                        .and_then(Effect::from_str)
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => bail!("signature effects must be an array {signature_hash}"),
        };
        normalize_effects(&effects)
    }

    pub(crate) fn signature_effect_names(&self, signature_hash: &str) -> Result<Vec<String>> {
        let effects = self.signature_effects(signature_hash)?;
        Ok(visible_effects(&effects)
            .into_iter()
            .map(|effect| effect.as_str().to_string())
            .collect())
    }

    pub(crate) fn signature_region_params(
        &self,
        signature_hash: &str,
    ) -> Result<Vec<RegionParamDef>> {
        let payload = self.get_payload(signature_hash)?;
        region_params_from_payload(payload.get("region_params"))
    }

    pub(crate) fn put_symbol_birth(
        &mut self,
        parent_history_hash: Option<&str>,
        birth_seed: &str,
    ) -> Result<String> {
        self.put_symbol_birth_with_kind(parent_history_hash, birth_seed, "function")
    }

    pub(crate) fn put_symbol_birth_with_kind(
        &mut self,
        parent_history_hash: Option<&str>,
        birth_seed: &str,
        symbol_kind: &str,
    ) -> Result<String> {
        self.put_object(
            "SymbolBirth",
            &json!({
                "symbol_kind": symbol_kind,
                "birth_history_hash": parent_history_hash.unwrap_or("genesis"),
                "local_nonce": birth_seed,
            }),
        )
    }

    pub(crate) fn put_function_def(
        &mut self,
        symbol: &str,
        signature: &str,
        body: &str,
    ) -> Result<String> {
        self.put_object(
            "FunctionDef",
            &json!({
                "symbol": symbol,
                "function_sig_hash": signature,
                "typed_body_expr_hash": body,
            }),
        )
    }

    /// Build the content-addressed `RecursionGroup` object for a mutually-recursive
    /// clique (SPEC_V3 §6). Members are sorted by symbol so the clique's identity
    /// is order-independent (the clique IS its member set). Each member entry
    /// carries its stable `symbol` identity plus its `definition`/`signature`, so
    /// the group's content hash covers the members' bodies — and those bodies
    /// reference in-group peers by their stable SymbolBirth hash (a by-name
    /// fixpoint edge, never a body-content edge, so the clique stays acyclic).
    pub(crate) fn put_recursion_group(
        &mut self,
        module: &str,
        members: &[crate::model::RootSymbolPayload],
    ) -> Result<String> {
        let mut entries = members
            .iter()
            .map(|member| {
                json!({
                    "symbol": member.symbol,
                    "definition": member.definition,
                    "signature": member.signature,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            a["symbol"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["symbol"].as_str().unwrap_or_default())
        });
        self.put_object(
            "RecursionGroup",
            &json!({
                "module": module,
                "members": entries,
            }),
        )
    }

    pub(crate) fn put_type_recursion_group(
        &mut self,
        module: &str,
        members: &[crate::model::RootTypePayload],
    ) -> Result<String> {
        let mut entries = members
            .iter()
            .map(|member| {
                json!({
                    "type_symbol": member.type_symbol,
                    "type_def": member.type_def,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            a["type_symbol"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["type_symbol"].as_str().unwrap_or_default())
        });
        self.put_object(
            "TypeRecursionGroup",
            &json!({
                "module": module,
                "members": entries,
            }),
        )
    }

    pub(crate) fn put_external_function(
        &mut self,
        symbol: &str,
        signature: &str,
        abi: &str,
        link_name: &str,
        library: Option<&str>,
    ) -> Result<String> {
        validate_external_abi_tag(abi)?;
        validate_external_link_name(link_name)?;
        if let Some(library) = library {
            validate_external_library_name(library)?;
        }
        self.validate_external_signature_effects(None, signature)?;
        let mut payload = serde_json::Map::new();
        payload.insert("symbol".to_string(), JsonValue::String(symbol.to_string()));
        payload.insert(
            "function_sig_hash".to_string(),
            JsonValue::String(signature.to_string()),
        );
        payload.insert("abi".to_string(), JsonValue::String(abi.to_string()));
        payload.insert(
            "link_name".to_string(),
            JsonValue::String(link_name.to_string()),
        );
        if let Some(library) = library {
            payload.insert(
                "library".to_string(),
                JsonValue::String(library.to_string()),
            );
        }
        self.put_object("ExternalFunction", &JsonValue::Object(payload))
    }

    /// Validate that an extern's declared effects cover its signature: every
    /// extern needs `ffi`, and any raw pointer reachable through an argument or
    /// return additionally needs `unsafe`.
    ///
    /// Two-tier by design. The creation-time call (`put_external_function`,
    /// `root = None`) is a best-effort fast check: without a root it can only
    /// resolve *structural* types, so a raw pointer hidden inside a `Named`
    /// record/enum is invisible there (`type_contains_raw_pointer`'s `Named`
    /// arm returns `false`). The AUTHORITATIVE check runs at every commit and at
    /// `verify` from inside `type_check_root` with `root = Some(..)`, which
    /// resolves `Named` → `Record`/`Enum` and so sees raw pointers nested behind
    /// named types. No extern reaches a committed root without passing the
    /// authoritative tier, so the creation-time gap cannot admit an
    /// effect-underdeclared extern; it only makes the early diagnostic less
    /// eager for named pointees. (Regression-locked by the named-pointee extern
    /// tests in `tests/raw_pointers.rs`.)
    fn validate_external_signature_effects(
        &self,
        root: Option<&ProgramRootPayload>,
        signature: &str,
    ) -> Result<()> {
        let effects = self.signature_effects(signature)?;
        // Only `ffi` (always) and `unsafe` (raw-pointer signatures, below) are
        // derived structurally for an extern. `io` and `alloc` are TRUSTED
        // annotations: the compiler has no ground truth that `write` performs I/O
        // or that `malloc` allocates, so it cannot synthesize those effects from
        // the signature. Capability/effect reporting for I/O and allocation
        // therefore rests on the `std.platform.*` extern declarations being
        // annotated correctly. This is a trusted-base property, not a wrapper
        // escape: a wrapper can never drop an effect its callee declares (see
        // `verify_function_effects`), but a mis-annotated extern would let its
        // callers under-report `io`/`alloc`.
        if !effects.contains(&Effect::Ffi) {
            bail!("external functions must declare the ffi effect");
        }
        if self.signature_uses_raw_pointer(root, signature)? && !effects.contains(&Effect::Unsafe) {
            bail!(
                "external functions with raw pointer arguments or returns must declare the unsafe effect"
            );
        }
        Ok(())
    }

    fn signature_uses_raw_pointer(
        &self,
        root: Option<&ProgramRootPayload>,
        signature: &str,
    ) -> Result<bool> {
        let (params, return_type) = self.signature_parts(signature)?;
        for param in params {
            if self.type_contains_raw_pointer(root, &param, &mut BTreeSet::new())? {
                return Ok(true);
            }
        }
        self.type_contains_raw_pointer(root, &return_type, &mut BTreeSet::new())
    }

    fn type_contains_raw_pointer(
        &self,
        root: Option<&ProgramRootPayload>,
        type_hash: &str,
        active_types: &mut BTreeSet<String>,
    ) -> Result<bool> {
        if !active_types.insert(type_hash.to_string()) {
            return Ok(false);
        }
        let spec = match root {
            Some(root) => self.type_spec_in_root(root, type_hash)?,
            None => self.type_spec(type_hash)?,
        };
        let contains = match spec {
            TypeSpec::RawPointer { .. } => Ok(true),
            TypeSpec::Reference { referent, .. } => {
                self.type_contains_raw_pointer(root, &referent, active_types)
            }
            TypeSpec::Box { element } => {
                self.type_contains_raw_pointer(root, &element, active_types)
            }
            TypeSpec::Vec { element } => {
                self.type_contains_raw_pointer(root, &element, active_types)
            }
            TypeSpec::String => Ok(false),
            TypeSpec::Slice { element, .. } => {
                self.type_contains_raw_pointer(root, &element, active_types)
            }
            TypeSpec::FixedArray { element, .. } => {
                self.type_contains_raw_pointer(root, &element, active_types)
            }
            TypeSpec::Record(fields) | TypeSpec::Enum(fields) => {
                let mut contains = false;
                for field in fields {
                    if self.type_contains_raw_pointer(root, &field.type_hash, active_types)? {
                        contains = true;
                        break;
                    }
                }
                Ok(contains)
            }
            // A `Named` type is only reachable here when `root` is `None`
            // (creation-time best-effort tier): with a root, `type_spec_in_root`
            // already expanded it to `Record`/`Enum` above. Returning `false`
            // here is safe because the authoritative root-aware tier in
            // `type_check_root` re-runs this check with the name resolved — see
            // `validate_external_signature_effects`.
            TypeSpec::Builtin(_) | TypeSpec::Named { .. } | TypeSpec::TypeParam { .. } => Ok(false),
        };
        active_types.remove(type_hash);
        contains
    }

    pub(crate) fn function_body_hash(&self, definition_hash: &str) -> Result<String> {
        let payload = self.get_payload(definition_hash)?;
        payload
            .get("typed_body_expr_hash")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("function definition missing typed_body_expr_hash"))
    }

    pub(crate) fn function_signature_hash(&self, definition_hash: &str) -> Result<String> {
        self.definition_signature_hash(definition_hash)
    }

    pub(crate) fn definition_signature_hash(&self, definition_hash: &str) -> Result<String> {
        let payload = self.get_payload(definition_hash)?;
        payload
            .get("function_sig_hash")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("definition missing function_sig_hash"))
    }

    pub(crate) fn definition_is_external(&self, definition_hash: &str) -> Result<bool> {
        Ok(self.get_kind(definition_hash)? == "ExternalFunction")
    }

    pub(crate) fn definition_is_internal_function(&self, definition_hash: &str) -> Result<bool> {
        Ok(self.get_kind(definition_hash)? == "FunctionDef")
    }

    pub(crate) fn external_function_metadata(
        &self,
        definition_hash: &str,
    ) -> Result<ExternalFunctionMetadata> {
        let kind = self.get_kind(definition_hash)?;
        if kind != "ExternalFunction" {
            bail!("definition is not ExternalFunction {definition_hash}");
        }
        let payload = self.get_payload(definition_hash)?;
        let symbol = payload
            .get("symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("external function missing symbol"))?
            .to_string();
        let signature = payload
            .get("function_sig_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("external function missing function_sig_hash"))?
            .to_string();
        let abi = payload
            .get("abi")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("external function missing abi"))?
            .to_string();
        let link_name = payload
            .get("link_name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("external function missing link_name"))?
            .to_string();
        let library = match payload.get("library") {
            Some(JsonValue::String(value)) => Some(value.clone()),
            Some(_) => bail!("external function library must be a string"),
            None => None,
        };
        validate_external_abi_tag(&abi)?;
        validate_external_link_name(&link_name)?;
        if let Some(library) = &library {
            validate_external_library_name(library)?;
        }
        Ok(ExternalFunctionMetadata {
            symbol,
            signature,
            abi,
            link_name,
            library,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn type_expr(
        &mut self,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
    ) -> Result<TypeCheckResult> {
        self.type_expr_in_module(MAIN_BRANCH, expr, root, param_names, param_types)
    }

    pub(crate) fn type_expr_in_module(
        &mut self,
        current_module: &str,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
    ) -> Result<TypeCheckResult> {
        self.type_expr_in_module_with_regions(
            current_module,
            expr,
            root,
            param_names,
            param_types,
            &BTreeMap::new(),
        )
    }

    pub(crate) fn type_expr_in_module_with_regions(
        &mut self,
        current_module: &str,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
    ) -> Result<TypeCheckResult> {
        self.type_expr_in_module_with_regions_expecting(
            current_module,
            expr,
            root,
            param_names,
            param_types,
            region_scope,
            None,
        )
    }

    /// Type an expression that flows into a known destination type (e.g. a
    /// function body checked against its return type). When `expected_type` is
    /// supplied and the expression is a top-level `fold`, the accumulator is
    /// anchored to that type so a record-literal accumulator builds in the
    /// destination (declaration-order) layout — the same anchoring the
    /// `let`-binding and call-argument paths apply (see `type_fold_expr`). All
    /// other expression forms type identically to the unanchored entry point.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn type_expr_in_module_with_regions_expecting(
        &mut self,
        current_module: &str,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        expected_type: Option<&str>,
    ) -> Result<TypeCheckResult> {
        // The root of a top-level type-check (a function body, or any expression
        // typed against a destination) is a result position, so an early `return`
        // (R7) is well-formed at its block boundaries; reject one placed in an
        // operand/value position before typing (see `validate_return_positions`).
        crate::expr::validate_return_positions(expr, true)?;
        self.type_expr_with_locals_expecting(
            current_module,
            expr,
            root,
            param_names,
            param_types,
            region_scope,
            &mut Vec::new(),
            expected_type,
        )
    }

    /// Type-check a `fold` expression. When `expected_acc_type` is supplied (the
    /// declared type of an enclosing `let` binding) and the inferred init type is
    /// assignable to it, the accumulator is anchored to that named type so the
    /// fold builds its accumulator directly in the destination layout.
    ///
    /// Without this, a bare record-literal init (e.g. `with acc = {b: 0, a: 0}`)
    /// infers a *structural* (alphabetically ordered) record type. The fold result
    /// would then need a layout-incompatible blind copy into a non-alphabetical
    /// named binding, which fails closed at native lowering and forces an explicit
    /// `let init: T = <literal>` workaround. Anchoring keeps non-alphabetical
    /// record accumulators native-buildable. With no enclosing annotation
    /// (`expected_acc_type` = None) the inferred init type is used unchanged.
    #[allow(clippy::too_many_arguments)]
    fn type_fold_expr(
        &mut self,
        current_module: &str,
        item: &str,
        target: &RawExpr,
        acc: &str,
        init: &RawExpr,
        body: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
        expected_acc_type: Option<&str>,
    ) -> Result<TypeCheckResult> {
        validate_projection_identifier("fold item binding", item)?;
        validate_projection_identifier("fold accumulator binding", acc)?;
        if item == acc {
            bail!("fold item and accumulator bindings must be distinct");
        }
        let target = self.type_expr_with_locals(
            current_module,
            target,
            root,
            param_names,
            param_types,
            region_scope,
            locals,
        )?;
        let info = self.indexed_element_type_in_root(root, &target.type_hash)?;
        let (target_kind, element_type, len) = match info {
            IndexedElementInfo::FixedArray {
                element_type, len, ..
            } => {
                if !self.typed_expr_is_place(&target.expr_hash)? {
                    bail!("fold over a fixed array requires an addressable array place");
                }
                ("fixed_array", element_type, Some(len))
            }
            IndexedElementInfo::Slice { element_type, .. } => ("slice", element_type, None),
        };
        let element_class = self.value_class_in_root(root, &element_type)?;
        if element_class.copy_kind == ValueCopyKind::MoveOnly {
            bail!("fold element type must be copyable in phase 13");
        }
        if element_class.contains_reference {
            bail!("fold element type must not carry references in phase 13");
        }
        let init = self.type_expr_with_locals(
            current_module,
            init,
            root,
            param_names,
            param_types,
            region_scope,
            locals,
        )?;
        // Anchor the accumulator to the enclosing binding's declared type when the
        // init is assignable to it, so a structural record-literal init builds in
        // the destination (named, declaration-order) layout instead of its sorted
        // structural layout. Falls back to the inferred init type otherwise, which
        // keeps the outer assignability check (and any layout mismatch) intact.
        let acc_type = match expected_acc_type {
            Some(expected)
                if expected != init.type_hash.as_str()
                    && self.type_assignable_in_root(root, &init.type_hash, expected)? =>
            {
                expected.to_string()
            }
            _ => init.type_hash.clone(),
        };
        let acc_class = self.value_class_in_root(root, &acc_type)?;
        if acc_class.copy_kind == ValueCopyKind::MoveOnly {
            bail!("fold accumulator type must be copyable in phase 13");
        }
        if acc_class.contains_reference {
            bail!("fold accumulator type must not carry references in phase 13");
        }
        locals.push(LocalTypeBinding {
            name: item.to_string(),
            type_hash: element_type.clone(),
        });
        locals.push(LocalTypeBinding {
            name: acc.to_string(),
            type_hash: acc_type.clone(),
        });
        // The body is a result position of type `acc_type`, so anchor a fold (or
        // let-tail fold) in body position to it — this makes a nested fold whose
        // accumulator is a non-alphabetical record native-buildable.
        let body = self.type_expr_with_locals_expecting(
            current_module,
            body,
            root,
            param_names,
            param_types,
            region_scope,
            locals,
            Some(acc_type.as_str()),
        );
        locals.pop();
        locals.pop();
        let body = body?;
        if !self.type_assignable_in_root(root, &body.type_hash, &acc_type)? {
            bail!(
                "fold body returns {}, expected accumulator type {}",
                self.type_name(&body.type_hash)?,
                self.type_name(&acc_type)?
            );
        }
        let mut payload = json!({
            "expr_kind": "fold",
            "item_name": item,
            "acc_name": acc,
            "target": target.expr_hash,
            "target_type": target.type_hash,
            "target_kind": target_kind,
            "element_type": element_type,
            "init": init.expr_hash,
            "acc_type": acc_type,
            "body": body.expr_hash,
            "type": acc_type,
        });
        if let Some(len) = len {
            payload["len"] = JsonValue::from(len);
        }
        let expr_hash = self.put_object("Expression", &payload)?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": acc_type }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash: acc_type,
        })
    }

    /// Type-check `loop acc = init while cond do body` (R8): the condition-driven
    /// counterpart of `type_fold_expr`. `acc` is bound (to `init`'s type, anchored
    /// to the enclosing destination type like a fold so a record-literal init builds
    /// in declaration-order layout) and is in scope for `cond` (which must be `bool`)
    /// and `body` (which must produce the next accumulator, same type). The
    /// accumulator may be move-only (loop-carried drop glue, SPEC_V3 §7): each
    /// iteration the body either consumes it wholly (producing the next value)
    /// or lowering drops the old value before the back-edge store — exactly
    /// once either way. It must not carry references (a loan cannot outlive
    /// the iteration that minted it). The loop's result type is the
    /// accumulator type (the final accumulator when `cond` is false).
    #[allow(clippy::too_many_arguments)]
    fn type_loop_expr(
        &mut self,
        current_module: &str,
        acc: &str,
        init: &RawExpr,
        cond: &RawExpr,
        body: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
        expected_acc_type: Option<&str>,
    ) -> Result<TypeCheckResult> {
        validate_projection_identifier("loop accumulator binding", acc)?;
        // `init` is checked with `acc` NOT yet in scope (acc is bound after init),
        // but with the enclosing destination type as its expected type — so a
        // record-literal init anchors to the named accumulator type (building in
        // declaration-order layout AND context-typing a sized-int field like
        // `{ acc: 0x0 }` to its declared width), mirroring `let acc: T = <init>`.
        let init = self.type_expr_with_locals_expecting(
            current_module,
            init,
            root,
            param_names,
            param_types,
            region_scope,
            locals,
            expected_acc_type,
        )?;
        let acc_type = match expected_acc_type {
            Some(expected)
                if expected != init.type_hash.as_str()
                    && self.type_assignable_in_root(root, &init.type_hash, expected)? =>
            {
                expected.to_string()
            }
            _ => init.type_hash.clone(),
        };
        let acc_class = self.value_class_in_root(root, &acc_type)?;
        if acc_class.contains_reference {
            bail!("loop accumulator type must not carry references");
        }
        locals.push(LocalTypeBinding {
            name: acc.to_string(),
            type_hash: acc_type.clone(),
        });
        // `cond` and `body` both see `acc`. Check both with it in scope, then pop
        // before propagating any error so the local stack stays consistent.
        let cond = self.type_expr_with_locals(
            current_module,
            cond,
            root,
            param_names,
            param_types,
            region_scope,
            locals,
        );
        let body = self.type_expr_with_locals_expecting(
            current_module,
            body,
            root,
            param_names,
            param_types,
            region_scope,
            locals,
            Some(acc_type.as_str()),
        );
        locals.pop();
        let cond = cond?;
        let body = body?;
        require_type(&cond.type_hash, &type_hash_for("Bool"), "loop condition", self)?;
        if !self.type_assignable_in_root(root, &body.type_hash, &acc_type)? {
            bail!(
                "loop body returns {}, expected accumulator type {}",
                self.type_name(&body.type_hash)?,
                self.type_name(&acc_type)?
            );
        }
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "loop",
                "acc_name": acc,
                "init": init.expr_hash,
                "acc_type": acc_type,
                "cond": cond.expr_hash,
                "body": body.expr_hash,
                "type": acc_type,
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": acc_type }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash: acc_type,
        })
    }

    #[allow(clippy::too_many_arguments)]
    /// Resolve the type of an integer literal `value` given the position's
    /// `expected_type`. A sized-integer expectation gives the literal that width
    /// (range-checked against it); anything else (no hint, an `i64` hint, or a
    /// non-integer hint) makes it `i64` — a non-integer expectation then surfaces as
    /// a normal type mismatch at the use site, not here (R5, context-typed literals).
    fn literal_int_type(&self, value: &str, expected_type: Option<&str>) -> Result<String> {
        if let Some(expected) = expected_type
            && let TypeSpec::Builtin(name) = self.type_spec(expected)?
            && let Some(int) = scalar_int_type(&name)
            && int.name != "I64"
        {
            if !int_literal_in_range(value, int) {
                bail!(
                    "integer literal {value} is out of range for {}",
                    int.name.to_ascii_lowercase()
                );
            }
            return Ok(expected.to_string());
        }
        // Default width is i64; accept a `0x` hex literal here too (the sized path
        // above already does, via `int_literal_in_range`).
        let i64_int = scalar_int_type("I64").expect("I64 is a scalar int");
        if !int_literal_in_range(value, i64_int) {
            bail!("invalid i64 literal {value}");
        }
        Ok(type_hash_for("I64"))
    }

    #[allow(clippy::too_many_arguments)]
    fn type_expr_with_locals(
        &mut self,
        current_module: &str,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
    ) -> Result<TypeCheckResult> {
        self.type_expr_with_locals_expecting(
            current_module,
            expr,
            root,
            param_names,
            param_types,
            region_scope,
            locals,
            None,
        )
    }

    /// Type an expression carrying an optional `expected_type` for the result
    /// position. `fold` uses it to anchor its accumulator to the destination
    /// layout (see `type_fold_expr`), and `vec_new` uses it to infer its element
    /// type. The hint propagates through `let ... in <tail>` so tail-position
    /// constructs see the enclosing destination type.
    #[allow(clippy::too_many_arguments)]
    fn type_expr_with_locals_expecting(
        &mut self,
        current_module: &str,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
        expected_type: Option<&str>,
    ) -> Result<TypeCheckResult> {
        match expr {
            RawExpr::LiteralI64 { value } => {
                // Context-typed integer literal (R5): when the result position
                // expects a sized integer type, the literal takes that width (range-
                // checked); otherwise it defaults to `i64`. The typed node keeps
                // `expr_kind: "literal_i64"` — an integer literal whose WIDTH is given
                // by its `type` field — so projection/diff/patch (which only read the
                // numeric text) are unchanged and only typing+eval+lowering dispatch
                // on width.
                let type_hash = self.literal_int_type(value, expected_type)?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "literal_i64",
                        "value": value,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::LiteralBool { value } => {
                let type_hash = type_hash_for("Bool");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "literal_bool",
                        "value": value,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::LiteralString { value } => {
                let bytes_hex = bytes_to_hex(value.as_bytes());
                self.type_static_bytes_literal(bytes_hex, "string")
            }
            RawExpr::LiteralBytes { bytes_hex } => {
                let bytes = hex_to_bytes(bytes_hex)?;
                self.type_static_bytes_literal(bytes_to_hex(&bytes), "bytes")
            }
            RawExpr::Unit => {
                let type_hash = type_hash_for("Unit");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "literal_unit",
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::ParamRef { index } => {
                let type_hash = param_types
                    .get(*index)
                    .cloned()
                    .ok_or_else(|| anyhow!("parameter index out of bounds: {index}"))?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "param_ref",
                        "index": index,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::ParamName { name } => {
                if let Some((base, fields)) = name.split_once('.') {
                    let mut typed = self.type_expr_with_locals(
                        current_module,
                        &RawExpr::ParamName {
                            name: base.to_string(),
                        },
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    )?;
                    for field in fields.split('.') {
                        typed = self.type_field_access(root, &typed, field)?;
                    }
                    return Ok(typed);
                }
                if let Some((depth, binding)) = local_binding_at_name(locals, name) {
                    let type_hash = binding.type_hash.clone();
                    let expr_hash = self.put_object(
                        "Expression",
                        &json!({
                            "expr_kind": "local_ref",
                            "depth": depth,
                            "type": type_hash,
                        }),
                    )?;
                    self.write_cache_json(
                        &expr_hash,
                        "typechecker",
                        "typed-dag",
                        ArtifactKind::TypedExpression,
                        &json!({ "type": type_hash }),
                    )?;
                    Ok(TypeCheckResult {
                        expr_hash,
                        type_hash,
                    })
                } else {
                    let index = param_names
                        .iter()
                        .position(|candidate| candidate == name)
                        .ok_or_else(|| anyhow!("unknown parameter {name}"))?;
                    self.type_expr_with_locals(
                        current_module,
                        &RawExpr::ParamRef { index },
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    )
                }
            }
            RawExpr::Call { name, args } => {
                if matches!(
                    name.as_str(),
                    "raw_ptr" | "raw_mut_ptr" | "raw_load" | "raw_store"
                ) {
                    return self.type_builtin_raw_pointer_call(
                        current_module,
                        name,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    );
                }
                if matches!(name.as_str(), "slice" | "mut_slice" | "len" | "subslice") {
                    return self.type_builtin_slice_call(
                        current_module,
                        name,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    );
                }
                if matches!(
                    name.as_str(),
                    "vec_new"
                        | "vec_push"
                        | "vec_get"
                        | "vec_len"
                        | "string_new"
                        | "string_len"
                        | "string_with_capacity"
                        | "string_push"
                        | "string_get"
                        | "string_set"
                ) {
                    return self.type_builtin_dynamic_buffer_call(
                        current_module,
                        name,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                        expected_type,
                    );
                }
                if matches!(name.as_str(), "arg_count" | "arg_len" | "arg_byte") {
                    return self.type_builtin_process_arg(
                        current_module,
                        name,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    );
                }
                if name == "array_set" {
                    return self.type_builtin_array_set(
                        current_module,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                        expected_type,
                    );
                }
                if name == "box_new" {
                    return self.type_builtin_box_new(
                        current_module,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    );
                }
                if name == "unbox" {
                    return self.type_builtin_unbox(
                        current_module,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    );
                }
                if let Some(target) = int_cast_target(name) {
                    return self.type_builtin_int_cast(
                        current_module,
                        target.name,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    );
                }
                let symbol = resolve_function_name_in_root(root, current_module, name)
                    .ok_or_else(|| anyhow!("unknown function {name}"))?;
                let callee = self
                    .root_symbol(root, &symbol)
                    .ok_or_else(|| anyhow!("function {name} missing symbol entry"))?;
                let (expected_params, return_type) = self.signature_parts(&callee.signature)?;
                if expected_params.len() != args.len() {
                    bail!(
                        "call to {name} expects {} args, got {}",
                        expected_params.len(),
                        args.len()
                    );
                }
                let type_param_count = self.signature_type_params(&callee.signature)?.len();
                let callee_regions = self
                    .signature_region_params(&callee.signature)?
                    .into_iter()
                    .map(|param| param.region)
                    .collect::<BTreeSet<_>>();
                if type_param_count > 0 {
                    return self.type_generic_call(
                        current_module,
                        name,
                        &symbol,
                        &expected_params,
                        &return_type,
                        type_param_count,
                        &callee_regions,
                        args,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                        expected_type,
                    );
                }
                let mut typed_args = Vec::with_capacity(args.len());
                let mut region_substitutions = BTreeMap::new();
                for (idx, arg) in args.iter().enumerate() {
                    // Anchor a `fold` argument (including one in `let ... in` tail
                    // position) to the callee's parameter type so a record-literal
                    // accumulator builds in the destination (declaration-order)
                    // layout, mirroring the `let`-binding path (see
                    // `type_fold_expr`). Non-fold args ignore the hint.
                    let typed = self.type_expr_with_locals_expecting(
                        current_module,
                        arg,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                        Some(&expected_params[idx]),
                    )?;
                    if !self.type_assignable_for_call_in_root(
                        root,
                        &typed.type_hash,
                        &expected_params[idx],
                        &callee_regions,
                    )? {
                        bail!(
                            "call arg {} for {name} expected {}, got {}",
                            idx,
                            self.type_name(&expected_params[idx])?,
                            self.type_name(&typed.type_hash)?
                        );
                    }
                    self.infer_call_region_substitutions(
                        root,
                        &typed.type_hash,
                        &expected_params[idx],
                        &callee_regions,
                        &mut region_substitutions,
                    )?;
                    typed_args.push(typed.expr_hash);
                }
                let return_type =
                    self.put_substituted_type_regions(&return_type, &region_substitutions)?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "call",
                        "symbol": symbol,
                        "args": typed_args,
                        "type": return_type,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": return_type }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: return_type,
                })
            }
            RawExpr::Binary { op, left, right } => {
                // Sized-integer operand inference (R5). For the arithmetic/bitwise/
                // shift operators (result type == operand type), a sized-integer
                // result expectation is pushed into the operands so two bare
                // literals take the width (`let m: u32 = 0xff00 | 0x00ff`). Then the
                // left operand's sized type becomes the expectation for the right
                // (`x >> 7`, `x & 0xff`); and if the left is a bare literal while the
                // right resolved to a sized integer, the left is re-typed at that
                // width (`32 - n`). A literal only adopts a width — non-literal
                // operands keep their own type, so a genuine width mismatch still
                // errors below. Comparisons/logical ops expect Bool, which must not
                // reach the operands, so they propagate nothing.
                let arith_like = matches!(
                    op.as_str(),
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>"
                );
                let operand_expected = match expected_type {
                    Some(t) if arith_like && scalar_int_type_by_hash(t).is_some() => Some(t),
                    _ => None,
                };
                let left_typed = self.type_expr_with_locals_expecting(
                    current_module,
                    left,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    operand_expected,
                )?;
                let right_expected = scalar_int_type_by_hash(&left_typed.type_hash)
                    .map(|_| left_typed.type_hash.clone())
                    .or_else(|| operand_expected.map(str::to_string));
                let right = self.type_expr_with_locals_expecting(
                    current_module,
                    right,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    right_expected.as_deref(),
                )?;
                let left = if matches!(left.as_ref(), RawExpr::LiteralI64 { .. })
                    && left_typed.type_hash != right.type_hash
                    && scalar_int_type_by_hash(&right.type_hash).is_some()
                {
                    self.type_expr_with_locals_expecting(
                        current_module,
                        left,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                        Some(&right.type_hash),
                    )?
                } else {
                    left_typed
                };
                let bool_hash = type_hash_for("Bool");
                let result_type = match op.as_str() {
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>" => {
                        if scalar_int_type_by_hash(&left.type_hash).is_none() {
                            bail!(
                                "integer operator {op} expected a sized integer left operand, got {}",
                                self.type_name(&left.type_hash)?
                            );
                        }
                        if scalar_int_type_by_hash(&right.type_hash).is_none() {
                            bail!(
                                "integer operator {op} expected a sized integer right operand, got {}",
                                self.type_name(&right.type_hash)?
                            );
                        }
                        if left.type_hash != right.type_hash {
                            bail!(
                                "integer operator {op} operands differ: {} vs {}",
                                self.type_name(&left.type_hash)?,
                                self.type_name(&right.type_hash)?
                            );
                        }
                        left.type_hash.clone()
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        if left.type_hash != right.type_hash {
                            bail!(
                                "comparison operands differ: {} vs {}",
                                self.type_name(&left.type_hash)?,
                                self.type_name(&right.type_hash)?
                            );
                        }
                        if scalar_int_type_by_hash(&left.type_hash).is_none() {
                            bail!(
                                "comparison operand expected a sized integer, got {}",
                                self.type_name(&left.type_hash)?
                            );
                        }
                        bool_hash
                    }
                    "&&" | "||" => {
                        require_type(&left.type_hash, &bool_hash, "left operand", self)?;
                        require_type(&right.type_hash, &bool_hash, "right operand", self)?;
                        bool_hash
                    }
                    _ => bail!("unsupported binary operator {op}"),
                };
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "binary",
                        "op": op,
                        "left": left.expr_hash,
                        "right": right.expr_hash,
                        "type": result_type,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": result_type }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: result_type,
                })
            }
            RawExpr::Unary { op, expr } => {
                // For the arithmetic unary operators, push the expected type into
                // the operand so a sized literal takes its width (`let x: i32 = -5`,
                // `~mask` where the result position is u32).
                let operand_expected = match op.as_str() {
                    "-" | "~" => expected_type,
                    _ => None,
                };
                // `-LITERAL` where the positive digits overflow the signed width
                // but the NEGATED value fits (exactly the MINs: `-128` as i8,
                // `-9223372036854775808`) folds into one negative literal node
                // (#9) — the positive half is unrepresentable, so the plain
                // Unary(literal) shape cannot type it. Previously-valid
                // programs never take this path (their positive literal is in
                // range), so existing typed hashes are unchanged; projection
                // prints the negative literal as `-N`, which re-parses right
                // back through this fold (round-trip stable).
                if op == "-"
                    && let RawExpr::LiteralI64 { value } = expr.as_ref()
                    && !value.starts_with("0x")
                    && !value.starts_with("0X")
                {
                    let target = expected_type
                        .and_then(|hash| match self.type_spec(hash) {
                            Ok(TypeSpec::Builtin(name)) => scalar_int_type(&name),
                            _ => None,
                        })
                        .filter(|int| int.signed)
                        .unwrap_or_else(|| scalar_int_type("I64").expect("I64 is scalar"));
                    let negated = format!("-{value}");
                    if !int_literal_in_range(value, target)
                        && int_literal_in_range(&negated, target)
                    {
                        return self.type_expr_with_locals_expecting(
                            current_module,
                            &RawExpr::LiteralI64 { value: negated },
                            root,
                            param_names,
                            param_types,
                            region_scope,
                            locals,
                            expected_type,
                        );
                    }
                }
                let typed = self.type_expr_with_locals_expecting(
                    current_module,
                    expr,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    operand_expected,
                )?;
                let bool_hash = type_hash_for("Bool");
                let result_type = match op.as_str() {
                    "-" | "~" => {
                        if scalar_int_type_by_hash(&typed.type_hash).is_none() {
                            bail!(
                                "unary operator {op} expected a sized integer operand, got {}",
                                self.type_name(&typed.type_hash)?
                            );
                        }
                        typed.type_hash.clone()
                    }
                    "!" => {
                        require_type(&typed.type_hash, &bool_hash, "unary operand", self)?;
                        bool_hash
                    }
                    _ => bail!("unsupported unary operator {op}"),
                };
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "unary",
                        "op": op,
                        "expr": typed.expr_hash,
                        "type": result_type,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": result_type }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: result_type,
                })
            }
            RawExpr::BorrowShared { region, target } => {
                let target = self.type_expr_with_locals(
                    current_module,
                    target,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_place(&target.expr_hash)? {
                    bail!("shared borrow target must be an addressable place");
                }
                let (region_name, region_hash) = match region {
                    Some(name) if name == "static" => ("static".to_string(), static_region_hash()),
                    Some(name) => (
                        name.clone(),
                        region_scope
                            .get(name)
                            .cloned()
                            .ok_or_else(|| anyhow!("unknown region parameter '{name}"))?,
                    ),
                    None if region_scope.len() == 1 => {
                        let (name, hash) = region_scope
                            .iter()
                            .next()
                            .expect("region_scope length was checked");
                        (name.clone(), hash.clone())
                    }
                    None => bail!(
                        "shared borrow requires an explicit region when the function has {} region parameters",
                        region_scope.len()
                    ),
                };
                let referent_type = match self.type_spec_in_root(root, &target.type_hash)? {
                    TypeSpec::Box { element } => element,
                    _ => target.type_hash.clone(),
                };
                let type_hash = self.put_structural_type(TypeSpec::Reference {
                    region: region_hash.clone(),
                    mutable: false,
                    referent: referent_type.clone(),
                })?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "borrow_shared",
                        "target": target.expr_hash,
                        "region": region_hash,
                        "region_name": region_name,
                        "target_type": target.type_hash,
                        "referent_type": referent_type,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::BorrowMut { region, target } => {
                let target = self.type_expr_with_locals(
                    current_module,
                    target,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_assignable_place(root, &target.expr_hash)? {
                    bail!("mutable borrow target must be a mutable semantic place");
                }
                let (region_name, region_hash) = match region {
                    Some(name) if name == "static" => bail!("mutable borrow cannot use 'static"),
                    Some(name) => (
                        name.clone(),
                        region_scope
                            .get(name)
                            .cloned()
                            .ok_or_else(|| anyhow!("unknown region parameter '{name}"))?,
                    ),
                    None if region_scope.len() == 1 => {
                        let (name, hash) = region_scope
                            .iter()
                            .next()
                            .expect("region_scope length was checked");
                        (name.clone(), hash.clone())
                    }
                    None => bail!(
                        "mutable borrow requires an explicit region when the function has {} region parameters",
                        region_scope.len()
                    ),
                };
                let referent_type = match self.type_spec_in_root(root, &target.type_hash)? {
                    TypeSpec::Box { element } => element,
                    _ => target.type_hash.clone(),
                };
                let type_hash = self.put_structural_type(TypeSpec::Reference {
                    region: region_hash.clone(),
                    mutable: true,
                    referent: referent_type.clone(),
                })?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "borrow_mut",
                        "target": target.expr_hash,
                        "region": region_hash,
                        "region_name": region_name,
                        "target_type": target.type_hash,
                        "referent_type": referent_type,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::Assign { target, value } => {
                let target = self.type_expr_with_locals(
                    current_module,
                    target,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_assignable_place(root, &target.expr_hash)? {
                    bail!("assignment target must be a mutable semantic place");
                }
                let value = self.type_expr_with_locals(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.type_assignable_in_root(root, &value.type_hash, &target.type_hash)? {
                    require_type(
                        &value.type_hash,
                        &target.type_hash,
                        "assignment value",
                        self,
                    )?;
                }
                let type_hash = type_hash_for("Unit");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "assign",
                        "target": target.expr_hash,
                        "value": value.expr_hash,
                        "target_type": target.type_hash,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::Let {
                name,
                ty,
                value,
                body,
            } => {
                validate_projection_identifier("let binding", name)?;
                let binding_type =
                    self.resolve_type_in_root_with_regions(current_module, root, ty, region_scope)?;
                // Pass the declared binding type into the value so constructs
                // needing a destination type (fold accumulators, vec_new element
                // inference) anchor to the declared layout/type.
                let value = self.type_expr_with_locals_expecting(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    Some(&binding_type),
                )?;
                if !self.type_assignable_in_root(root, &value.type_hash, &binding_type)? {
                    require_type(&value.type_hash, &binding_type, "let binding", self)?;
                }
                locals.push(LocalTypeBinding {
                    name: name.clone(),
                    type_hash: binding_type.clone(),
                });
                // The let body is in the same result position as the whole let, so
                // propagate `expected_type` into it: a fold in tail position
                // (`let xs = .. in fold ..`) anchors to the destination layout.
                let body = self.type_expr_with_locals_expecting(
                    current_module,
                    body,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    expected_type,
                );
                locals.pop();
                let body = body?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "let",
                        "binding_name": name,
                        "binding_type": binding_type,
                        "value": value.expr_hash,
                        "body": body.expr_hash,
                        "type": body.type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": body.type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: body.type_hash,
                })
            }
            RawExpr::If {
                cond,
                then_expr,
                else_expr,
            } => {
                // Early-exit divergence (R7): a branch that always `return`s yields
                // no value to the `if`, so its type need not match the other
                // branch — the non-divergent branch fixes the result type. Computed
                // from the raw branch shape before it is shadowed by its typed result.
                let then_diverges = crate::expr::raw_expr_diverges(then_expr);
                let else_diverges = crate::expr::raw_expr_diverges(else_expr);
                let cond = self.type_expr_with_locals(
                    current_module,
                    cond,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let bool_hash = type_hash_for("Bool");
                require_type(&cond.type_hash, &bool_hash, "if condition", self)?;
                let then_expr = self.type_expr_with_locals(
                    current_module,
                    then_expr,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let else_expr = self.type_expr_with_locals(
                    current_module,
                    else_expr,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                // The result type is the non-divergent branch's; if both produce a
                // value they must agree; if both diverge the `if` itself diverges
                // and its (unobserved) type is taken from `then`.
                let result_type = if then_diverges && !else_diverges {
                    else_expr.type_hash.clone()
                } else if else_diverges && !then_diverges {
                    then_expr.type_hash.clone()
                } else {
                    if !then_diverges && then_expr.type_hash != else_expr.type_hash {
                        bail!(
                            "if branches differ: {} vs {}",
                            self.type_name(&then_expr.type_hash)?,
                            self.type_name(&else_expr.type_hash)?
                        );
                    }
                    then_expr.type_hash.clone()
                };
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "if",
                        "cond": cond.expr_hash,
                        "then": then_expr.expr_hash,
                        "else": else_expr.expr_hash,
                        "type": result_type,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": result_type }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: result_type,
                })
            }
            RawExpr::Return { value } => {
                // `return <value>` (R7): early exit from the enclosing function.
                // It is divergent — it yields no value to its own context — so the
                // typed node carries the operand's type only so a sibling `if`/`case`
                // branch can fix the join's result type (see the `If`/`Case` arms).
                // Two facts are checked elsewhere, where the function's return type
                // is in scope: (1) the operand is assignable to the declared return
                // type (the `return` arm of `verify_expr_type_with_locals`, which
                // `type_check_root` runs on every apply and verify runs again);
                // (2) the `return` sits in a block-result position
                // (`validate_return_positions`). The operand is typed against the
                // ambient `expected_type`, which is the return type in tail position
                // so a context-typed literal (`return 0` for a sized-int return)
                // takes the right width.
                let value = self.type_expr_with_locals_expecting(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    expected_type,
                )?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "return",
                        "value": value.expr_hash,
                        "type": value.type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": value.type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: value.type_hash,
                })
            }
            RawExpr::Fold {
                item,
                target,
                acc,
                init,
                body,
            } => self.type_fold_expr(
                current_module,
                item,
                target,
                acc,
                init,
                body,
                root,
                param_names,
                param_types,
                region_scope,
                locals,
                expected_type,
            ),
            RawExpr::Loop {
                acc,
                init,
                cond,
                body,
            } => self.type_loop_expr(
                current_module,
                acc,
                init,
                cond,
                body,
                root,
                param_names,
                param_types,
                region_scope,
                locals,
                expected_type,
            ),
            RawExpr::Array { elements } => {
                if elements.is_empty() {
                    bail!("array literal must have at least one element");
                }
                // When the result position expects a fixed array, push its element
                // type into each element so sized-integer literals take that width
                // (`let w: array<u32, 64> = [0x61626380, 0, ..]`); otherwise an
                // element would default to i64 and the structural array would not be
                // assignable to the named element width.
                let expected_element = expected_type
                    .and_then(|hash| self.type_spec_in_root(root, hash).ok())
                    .and_then(|spec| match spec {
                        TypeSpec::FixedArray { element, .. } => Some(element),
                        _ => None,
                    });
                let mut typed_elements = Vec::with_capacity(elements.len());
                for element in elements {
                    typed_elements.push(self.type_expr_with_locals_expecting(
                        current_module,
                        element,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                        expected_element.as_deref(),
                    )?);
                }
                let element_type = typed_elements
                    .first()
                    .ok_or_else(|| anyhow!("array literal has no elements"))?
                    .type_hash
                    .clone();
                for (idx, typed) in typed_elements.iter().enumerate() {
                    if !self.type_assignable_in_root(root, &typed.type_hash, &element_type)? {
                        bail!(
                            "array element {idx} expected {}, got {}",
                            self.type_name(&element_type)?,
                            self.type_name(&typed.type_hash)?
                        );
                    }
                }
                let type_hash = self.put_structural_type(TypeSpec::FixedArray {
                    element: element_type.clone(),
                    len: typed_elements.len() as u64,
                })?;
                let elements_json = typed_elements
                    .iter()
                    .map(|typed| {
                        json!({
                            "value": typed.expr_hash.clone(),
                            "type": typed.type_hash.clone(),
                        })
                    })
                    .collect::<Vec<_>>();
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "array_literal",
                        "elements": elements_json,
                        "element_type": element_type.clone(),
                        "len": typed_elements.len() as u64,
                        "type": type_hash.clone(),
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::ArrayFill { value, count } => {
                // `[value; count]` (R9): `value` is evaluated once and replicated into
                // `count` slots, so its type must be Copy (replicating a move-only
                // value would mint `count` owners of one resource). `count` is a
                // non-negative integer literal; the result is `array<T, count>`.
                let count: u64 = count
                    .parse()
                    .map_err(|_| anyhow!("array fill count must be a non-negative integer"))?;
                if count == 0 {
                    bail!("array fill count must be at least 1");
                }
                // The count is replicated storage (eval allocates it, lowering
                // unrolls a store per slot), so an unbounded literal is a
                // resource bomb (#10) — capped fail-closed at import.
                if count > MAX_FIXED_ARRAY_LEN {
                    bail!(
                        "array fill count {count} exceeds the supported maximum {MAX_FIXED_ARRAY_LEN}"
                    );
                }
                let expected_element = expected_type
                    .and_then(|hash| self.type_spec_in_root(root, hash).ok())
                    .and_then(|spec| match spec {
                        TypeSpec::FixedArray { element, .. } => Some(element),
                        _ => None,
                    });
                let value = self.type_expr_with_locals_expecting(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    expected_element.as_deref(),
                )?;
                let element_type = value.type_hash.clone();
                // The value is replicated into `count` slots, so it must be Copy with
                // trivial drop and hold no reference (replicating a reference would
                // duplicate a loan into every slot; a move-only value would mint
                // `count` owners). This mirrors the dynamic-buffer element discipline
                // and keeps the array-fill borrow/move analyses trivial.
                let class = self.value_class_in_root(root, &element_type)?;
                if class.copy_kind != ValueCopyKind::Copy
                    || class.drop_kind != ValueDropKind::Trivial
                    || class.contains_reference
                {
                    bail!(
                        "array fill value must be a non-reference Copy value with trivial drop \
                         (it is replicated {count} times), got {}",
                        self.type_name(&element_type)?
                    );
                }
                let type_hash = self.put_structural_type(TypeSpec::FixedArray {
                    element: element_type.clone(),
                    len: count,
                })?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "array_fill",
                        "value": value.expr_hash,
                        "element_type": element_type.clone(),
                        "count": count,
                        "type": type_hash.clone(),
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::Index { target, index } => {
                let target = self.type_expr_with_locals(
                    current_module,
                    target,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let index = self.type_expr_with_locals(
                    current_module,
                    index,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                self.type_array_index(root, &target, &index)
            }
            RawExpr::Record { fields } => {
                if fields.is_empty() {
                    bail!("record literal must have at least one field");
                }
                // When the result position expects a record type, push each field's
                // expected type into its value so a sized-integer literal in a field
                // takes that width (`let s: State = { a: 0x6a09e667, .. }` with
                // `State.a: u32`); otherwise the field would default to i64 and the
                // structural literal would not be assignable to the named record.
                let expected_fields: Option<BTreeMap<String, String>> = expected_type
                    .and_then(|hash| self.type_spec_in_root(root, hash).ok())
                    .and_then(|spec| match spec {
                        TypeSpec::Record(field_specs) => Some(
                            field_specs
                                .into_iter()
                                .map(|f| (f.name, f.type_hash))
                                .collect(),
                        ),
                        _ => None,
                    });
                let mut names = BTreeSet::new();
                let mut typed_values = Vec::with_capacity(fields.len());
                for field in fields {
                    validate_projection_identifier("record field", &field.name)?;
                    if !names.insert(field.name.clone()) {
                        bail!("duplicate record field {}", field.name);
                    }
                    let field_expected = expected_fields
                        .as_ref()
                        .and_then(|m| m.get(&field.name))
                        .map(String::as_str);
                    let typed = self.type_expr_with_locals_expecting(
                        current_module,
                        &field.value,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                        field_expected,
                    )?;
                    typed_values.push((field.name.clone(), typed));
                }
                let type_hash = self.put_structural_type(TypeSpec::Record(
                    typed_values
                        .iter()
                        .map(|(name, typed)| TypeFieldSpec {
                            name: name.clone(),
                            type_hash: typed.type_hash.clone(),
                        })
                        .collect(),
                ))?;
                let fields_json = typed_values
                    .iter()
                    .map(|(name, typed)| {
                        json!({
                            "name": name,
                            "value": typed.expr_hash,
                            "type": typed.type_hash,
                        })
                    })
                    .collect::<Vec<_>>();
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "record_literal",
                        "fields": fields_json,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::FieldAccess { target, field } => {
                validate_projection_identifier("record field", field)?;
                let target = self.type_expr_with_locals(
                    current_module,
                    target,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                self.type_field_access(root, &target, field)
            }
            RawExpr::EnumConstruct {
                enum_type,
                variant,
                value,
            } => {
                validate_projection_identifier("enum variant", variant)?;
                let enum_type_hash = self.resolve_enum_construct_type(
                    current_module,
                    root,
                    enum_type,
                    variant,
                    value,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    expected_type,
                )?;
                let variant_type =
                    self.enum_variant_type_in_root(root, &enum_type_hash, variant)?;
                // Propagate the variant's payload type as the expected type so a
                // record/enum/array literal (possibly under `box_new`) is built in
                // the payload's nominal layout instead of an anonymous structural
                // one — otherwise `E::v(box_new({ ... }))` fails the payload check.
                let typed_value = self.type_expr_with_locals_expecting(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    Some(&variant_type),
                )?;
                // Accept a structurally-compatible payload (e.g. an anonymous record
                // literal) for a nominal variant type, mirroring `let`-binding
                // coercion; lowering builds it in the variant's layout. Fall back to
                // the strict check for a precise diagnostic when not assignable.
                if !self
                    .type_assignable_in_root(root, &typed_value.type_hash, &variant_type)?
                {
                    require_type(
                        &typed_value.type_hash,
                        &variant_type,
                        "enum variant payload",
                        self,
                    )?;
                }
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "enum_construct",
                        "enum_type": enum_type_hash,
                        "variant": variant,
                        "value": typed_value.expr_hash,
                        "type": enum_type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": enum_type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: enum_type_hash,
                })
            }
            RawExpr::Case { expr, arms } => {
                let scrutinee = self.type_expr_with_locals(
                    current_module,
                    expr,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if arms.is_empty() {
                    bail!("case expression must have at least one arm");
                }
                // Scalar literal `case` (R14): dispatch on an `i64`/`bool`
                // scrutinee by literal patterns plus a `_` wildcard. Produces a
                // typed `case` node whose arms carry `literal_i64`/`literal_bool`;
                // lowering desugars it to an `if`/`eq` chain.
                let scalar_kind = if scrutinee.type_hash == type_hash_for("I64") {
                    Some(ScalarCaseKind::I64)
                } else if scrutinee.type_hash == type_hash_for("Bool") {
                    Some(ScalarCaseKind::Bool)
                } else {
                    None
                };
                if let Some(scalar_kind) = scalar_kind {
                    return self.type_scalar_case(
                        current_module,
                        &scrutinee,
                        scalar_kind,
                        arms,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    );
                }
                let TypeSpec::Enum(variants) =
                    self.type_spec_in_root(root, &scrutinee.type_hash)?
                else {
                    bail!(
                        "case expression requires an enum or scalar (i64/bool) scrutinee, got {}",
                        self.type_name(&scrutinee.type_hash)?
                    );
                };
                let mut seen = BTreeSet::new();
                // Per outer variant: `true` once it carries nested destructuring
                // arms, `false` once it carries a simple binding/no-binding arm. Used
                // to reject duplicate simple arms and mixing the two for one variant.
                let mut variant_kind: BTreeMap<String, bool> = BTreeMap::new();
                let mut has_nested = false;
                // Each non-default arm as a pattern over the *scrutinee* enum, for the
                // recursive nested-exhaustiveness check (R14).
                let mut arm_patterns: Vec<RawPattern> = Vec::new();
                let mut result_type: Option<String> = None;
                // The first arm's body type, used only when every arm diverges (R7).
                let mut fallback_type: Option<String> = None;
                let mut arms_json = Vec::with_capacity(arms.len());
                let mut has_default = false;
                for (index, arm) in arms.iter().enumerate() {
                    if arm.guard.is_some() {
                        bail!("if-guards are only supported on i64 scalar `case` arms");
                    }
                    let body;
                    if arm.default {
                        if index + 1 != arms.len() {
                            bail!("default case arm must be last");
                        }
                        if has_default {
                            bail!("duplicate default case arm");
                        }
                        if arm.variant.is_some() {
                            bail!("default case arm cannot specify a variant");
                        }
                        if arm.binding.is_some() {
                            bail!("default case arm cannot bind a payload");
                        }
                        has_default = true;
                        body = self.type_expr_with_locals(
                            current_module,
                            &arm.body,
                            root,
                            param_names,
                            param_types,
                            region_scope,
                            locals,
                        )?;
                        arms_json.push(json!({
                            "default": true,
                            "body": body.expr_hash,
                        }));
                    } else {
                        let variant = arm
                            .variant
                            .as_deref()
                            .ok_or_else(|| anyhow!("case arm missing variant"))?;
                        validate_projection_identifier("enum variant", variant)?;
                        let variant_type = variants
                            .iter()
                            .find(|candidate| candidate.name == variant)
                            .map(|candidate| candidate.type_hash.clone())
                            .ok_or_else(|| anyhow!("case arm uses unknown variant {variant}"))?;
                        // The (at most one) leaf binding the arm body sees in scope, and
                        // the typed nested pattern node (R14), if any.
                        let scoped: Option<(String, String)>;
                        let mut payload_pattern_json: Option<JsonValue> = None;
                        if let Some(pattern) = &arm.payload_pattern {
                            // Nested destructuring arm: `variant(inner(..))`. Multiple
                            // nested arms may share an outer variant (they dispatch on
                            // the payload); mixing with a simple binding is rejected.
                            has_nested = true;
                            match variant_kind.get(variant) {
                                Some(true) => {}
                                Some(false) => bail!(
                                    "case arm {variant} cannot mix a binding pattern with nested destructuring patterns"
                                ),
                                None => {
                                    variant_kind.insert(variant.to_string(), true);
                                }
                            }
                            let (node, leaf) =
                                self.type_payload_pattern(root, pattern, &variant_type)?;
                            payload_pattern_json = Some(node);
                            scoped = leaf;
                        } else {
                            // Simple arm: at most one per variant, never mixed with a
                            // nested arm.
                            match variant_kind.get(variant) {
                                Some(true) => bail!(
                                    "case arm {variant} cannot mix a binding pattern with nested destructuring patterns"
                                ),
                                Some(false) => bail!("duplicate case arm {variant}"),
                                None => {
                                    variant_kind.insert(variant.to_string(), false);
                                }
                            }
                            if let Some(binding) = &arm.binding {
                                validate_projection_identifier("case binding", binding)?;
                                scoped = Some((binding.clone(), variant_type.clone()));
                            } else if variant_type != type_hash_for("Unit") {
                                bail!("case arm {variant} must bind its payload");
                            } else {
                                scoped = None;
                            }
                        }
                        seen.insert(variant.to_string());
                        arm_patterns.push(arm_scrutinee_pattern(arm, variant));
                        if let Some((name, type_hash)) = &scoped {
                            locals.push(LocalTypeBinding {
                                name: name.clone(),
                                type_hash: type_hash.clone(),
                            });
                        }
                        let typed_body = self.type_expr_with_locals(
                            current_module,
                            &arm.body,
                            root,
                            param_names,
                            param_types,
                            region_scope,
                            locals,
                        );
                        if scoped.is_some() {
                            locals.pop();
                        }
                        body = typed_body?;
                        match payload_pattern_json {
                            Some(pattern) => arms_json.push(json!({
                                "variant": variant,
                                "payload_pattern": pattern,
                                "body": body.expr_hash,
                            })),
                            None => arms_json.push(json!({
                                "variant": arm.variant.as_deref(),
                                "binding_name": &arm.binding,
                                "body": body.expr_hash,
                            })),
                        }
                    }
                    // Early-exit divergence (R7): an arm whose body always `return`s
                    // yields no value to the `case`, so it neither fixes nor must
                    // match the result type — the non-divergent arms do (mirrors the
                    // `if` join). `fallback_type` keeps a type for the all-arms-diverge
                    // case (the `case` then diverges; its type is unobserved).
                    if fallback_type.is_none() {
                        fallback_type = Some(body.type_hash.clone());
                    }
                    if !crate::expr::raw_expr_diverges(&arm.body) {
                        if let Some(expected) = &result_type {
                            if expected != &body.type_hash {
                                bail!(
                                    "case arm returns {}, expected {}",
                                    self.type_name(&body.type_hash)?,
                                    self.type_name(expected)?
                                );
                            }
                        } else {
                            result_type = Some(body.type_hash.clone());
                        }
                    }
                }
                let expected_variants = variants
                    .iter()
                    .map(|variant| variant.name.clone())
                    .collect::<BTreeSet<_>>();
                if has_nested {
                    // Reject ill-formed pattern trees (mixing/duplicate at any level)
                    // so the decision-tree lowering sees homogeneous variant groups.
                    self.check_patterns_well_formed(root, &scrutinee.type_hash, &arm_patterns)?;
                }
                if !has_default {
                    if has_nested {
                        // Nested patterns: every variant — and within it every nested
                        // sub-pattern path — must be covered, recursively (R14).
                        if !self.patterns_exhaustive(root, &scrutinee.type_hash, &arm_patterns)? {
                            bail!(
                                "case expression is not exhaustive: every variant and nested sub-pattern must be covered, or add a `_` arm"
                            );
                        }
                    } else if seen != expected_variants {
                        bail!("case expression must cover every enum variant");
                    }
                }
                let type_hash = result_type
                    .or(fallback_type)
                    .ok_or_else(|| anyhow!("case expression has no arms"))?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "case",
                        "expr": scrutinee.expr_hash,
                        "arms": arms_json,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
        }
    }

    /// Type-check a nested destructuring payload pattern (R14) against the type it
    /// matches (`match_type`, an enum), returning the typed pattern node
    /// (`{"variant": v, "binding_name"?: x, "payload_pattern"?: {..}}`) plus the
    /// chain's single leaf binding `(name, type)` to put in scope for the arm body.
    fn type_payload_pattern(
        &self,
        root: &ProgramRootPayload,
        pattern: &RawPattern,
        match_type: &str,
    ) -> Result<(JsonValue, Option<(String, String)>)> {
        let RawPattern::Variant { variant, sub } = pattern else {
            // The parser only stores a `Variant` here (a `Binding`/`Wildcard`
            // becomes the arm's `binding`/no-binding); defensive.
            bail!("nested payload pattern must be a variant pattern");
        };
        let TypeSpec::Enum(variants) = self.type_spec_in_root(root, match_type)? else {
            bail!(
                "nested pattern `{variant}(..)` requires an enum payload, got {}",
                self.type_name(match_type)?
            );
        };
        validate_projection_identifier("enum variant", variant)?;
        let payload_type = variants
            .iter()
            .find(|candidate| &candidate.name == variant)
            .map(|candidate| candidate.type_hash.clone())
            .ok_or_else(|| anyhow!("nested pattern uses unknown variant {variant}"))?;
        let mut node = serde_json::Map::new();
        node.insert("variant".to_string(), json!(variant));
        let leaf = match sub.as_ref() {
            RawPattern::Binding(name) => {
                validate_projection_identifier("case binding", name)?;
                node.insert("binding_name".to_string(), json!(name));
                Some((name.clone(), payload_type))
            }
            RawPattern::Wildcard => None,
            inner @ RawPattern::Variant { .. } => {
                let (inner_node, leaf) = self.type_payload_pattern(root, inner, &payload_type)?;
                node.insert("payload_pattern".to_string(), inner_node);
                leaf
            }
        };
        Ok((JsonValue::Object(node), leaf))
    }

    /// Reject ill-formed nested pattern sets (R14): at any level a binding/wildcard
    /// catch-all cannot coexist with a variant pattern, nor two catch-alls — both
    /// would leave an arm dead (first-match) and make the decision tree ambiguous.
    /// Guarantees each variant group is homogeneous (one catch-all xor all-deeper),
    /// which the decision-tree lowering relies on.
    fn check_patterns_well_formed(
        &self,
        root: &ProgramRootPayload,
        match_type: &str,
        patterns: &[RawPattern],
    ) -> Result<()> {
        let catch_alls = patterns
            .iter()
            .filter(|pattern| matches!(pattern, RawPattern::Binding(_) | RawPattern::Wildcard))
            .count();
        let variant_patterns: Vec<&RawPattern> = patterns
            .iter()
            .filter(|pattern| matches!(pattern, RawPattern::Variant { .. }))
            .collect();
        if catch_alls > 1 {
            bail!("duplicate catch-all pattern: a binding or `_` already matches every value here");
        }
        if catch_alls >= 1 && !variant_patterns.is_empty() {
            bail!(
                "a binding/`_` pattern cannot be mixed with variant patterns at the same level (to match a nullary variant use `v(_)`)"
            );
        }
        if variant_patterns.is_empty() {
            return Ok(());
        }
        let TypeSpec::Enum(variants) = self.type_spec_in_root(root, match_type)? else {
            bail!("variant pattern requires an enum payload");
        };
        for variant in &variants {
            let subs: Vec<RawPattern> = variant_patterns
                .iter()
                .filter_map(|pattern| match pattern {
                    RawPattern::Variant { variant: name, sub } if name == &variant.name => {
                        Some((**sub).clone())
                    }
                    _ => None,
                })
                .collect();
            if !subs.is_empty() {
                self.check_patterns_well_formed(root, &variant.type_hash, &subs)?;
            }
        }
        Ok(())
    }

    /// Whether `patterns` (each matching `match_type`) cover every value of
    /// `match_type` (R14 nested exhaustiveness). A `Binding`/`Wildcard` is a
    /// catch-all; otherwise every variant of the enum must be covered recursively
    /// by the sub-patterns of the arms naming it. Terminates: each recursion
    /// descends into a strictly inner *inline* enum payload (a `box` breaks the
    /// type cycle and cannot itself be pattern-matched).
    fn patterns_exhaustive(
        &self,
        root: &ProgramRootPayload,
        match_type: &str,
        patterns: &[RawPattern],
    ) -> Result<bool> {
        if patterns
            .iter()
            .any(|pattern| matches!(pattern, RawPattern::Binding(_) | RawPattern::Wildcard))
        {
            return Ok(true);
        }
        let TypeSpec::Enum(variants) = self.type_spec_in_root(root, match_type)? else {
            // Only variant patterns remain but the type is not an enum — type
            // checking already rejected that; treat as non-exhaustive defensively.
            return Ok(false);
        };
        for variant in &variants {
            let subs: Vec<RawPattern> = patterns
                .iter()
                .filter_map(|pattern| match pattern {
                    RawPattern::Variant { variant: name, sub } if name == &variant.name => {
                        Some((**sub).clone())
                    }
                    _ => None,
                })
                .collect();
            if subs.is_empty() {
                return Ok(false);
            }
            if !self.patterns_exhaustive(root, &variant.type_hash, &subs)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Type-check a scalar literal `case` (R14): arms are scalar literal patterns
    /// (`i64`/`bool`) plus an optional `_` wildcard. Builds a typed `case` node
    /// whose arms carry `literal_i64`/`literal_bool` (or `default`); lowering
    /// desugars it to an `if`/`eq` chain. Exhaustiveness: an `i64` case must have
    /// a `_`; a `bool` case must cover `true` and `false` or have a `_`.
    #[allow(clippy::too_many_arguments)]
    fn type_scalar_case(
        &mut self,
        current_module: &str,
        scrutinee: &TypeCheckResult,
        kind: ScalarCaseKind,
        arms: &[RawCaseArm],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
    ) -> Result<TypeCheckResult> {
        let mut arms_json = Vec::with_capacity(arms.len());
        let mut result_type: Option<String> = None;
        // First arm's body type, used only when every arm diverges (R7).
        let mut fallback_type: Option<String> = None;
        let mut has_default = false;
        let mut seen_i64: BTreeSet<String> = BTreeSet::new();
        let mut seen_bool: BTreeSet<bool> = BTreeSet::new();
        for (index, arm) in arms.iter().enumerate() {
            if arm.variant.is_some() {
                bail!("scalar case arm cannot use a variant pattern; use a literal or `_`");
            }
            if arm.binding.is_some() {
                bail!("scalar case arm cannot bind a value");
            }
            if arm.guard.is_some() && matches!(kind, ScalarCaseKind::Bool) {
                bail!("if-guards require an i64 scrutinee");
            }
            let mut pattern = serde_json::Map::new();
            if arm.default {
                // A guarded wildcard (`_ if g`) is a conditional arm: it may appear
                // before other arms and never proves exhaustiveness. Only the
                // UNGUARDED catch-all must be unique, last, and prove exhaustiveness.
                if arm.guard.is_none() {
                    if index + 1 != arms.len() {
                        bail!("default case arm must be last");
                    }
                    if has_default {
                        bail!("duplicate default case arm");
                    }
                    has_default = true;
                }
                pattern.insert("default".to_string(), json!(true));
            } else if let Some(range) = &arm.range {
                // i64 range pattern (R14): `lo..hi` / `lo..=hi`. First-match
                // semantics — overlapping ranges are not flagged (a redundancy lint,
                // not a soundness concern). Exhaustiveness still requires a `_` arm
                // for i64 (a finite set of ranges cannot prove full i64 coverage).
                if !matches!(kind, ScalarCaseKind::I64) {
                    bail!("range case patterns require an i64 scrutinee");
                }
                let bound = |expr: &RawExpr| -> Result<i64> {
                    match expr {
                        RawExpr::LiteralI64 { value } => value
                            .parse::<i64>()
                            .with_context(|| format!("invalid i64 range bound {value}")),
                        _ => bail!("range case bound must be an integer literal"),
                    }
                };
                let lo = bound(&range.lo)?;
                let hi = bound(&range.hi)?;
                let empty = if range.inclusive { lo > hi } else { lo >= hi };
                if empty {
                    bail!(
                        "empty range case pattern {lo}..{}{hi}",
                        if range.inclusive { "=" } else { "" }
                    );
                }
                pattern.insert("range_lo".to_string(), json!(lo.to_string()));
                pattern.insert("range_hi".to_string(), json!(hi.to_string()));
                pattern.insert("range_inclusive".to_string(), json!(range.inclusive));
            } else {
                let literal = arm
                    .literal
                    .as_deref()
                    .ok_or_else(|| anyhow!("scalar case arm must be a literal or `_`"))?;
                match kind {
                    ScalarCaseKind::I64 => {
                        let RawExpr::LiteralI64 { value } = literal else {
                            bail!("scalar case pattern type does not match the i64 scrutinee");
                        };
                        value
                            .parse::<i64>()
                            .with_context(|| format!("invalid i64 case pattern {value}"))?;
                        // Only an UNGUARDED literal fully covers its value; a guarded
                        // one may legitimately repeat (its guard can fall through).
                        // Overlap with a later arm is first-match (a redundancy lint,
                        // not flagged — consistent with range overlaps).
                        if arm.guard.is_none() && !seen_i64.insert(value.clone()) {
                            bail!("duplicate case pattern {value}");
                        }
                        pattern.insert("literal_i64".to_string(), json!(value));
                    }
                    ScalarCaseKind::Bool => {
                        let RawExpr::LiteralBool { value } = literal else {
                            bail!("scalar case pattern type does not match the bool scrutinee");
                        };
                        if !seen_bool.insert(*value) {
                            bail!("duplicate case pattern {value}");
                        }
                        pattern.insert("literal_bool".to_string(), json!(value));
                    }
                }
            }
            // Type-check the `if` guard (R14): a `bool` predicate in the arm's scope.
            // The guard is SHORT-CIRCUITED (evaluated only when the pattern matched, in
            // source order), so eval and the native chain agree even on a trapping or
            // effectful guard; any effect it uses is accounted in the enclosing
            // function's effect signature (inline via `expr_requires_*`, call-borne via
            // the dependency graph). The one runtime hazard — moving an owned value out
            // of the arm's scope in a guard — is rejected at lowering. A guarded arm
            // never proves exhaustiveness (handled above).
            if let Some(guard_expr) = &arm.guard {
                let guard = self.type_expr_with_locals(
                    current_module,
                    guard_expr,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if guard.type_hash != type_hash_for("Bool") {
                    bail!(
                        "case guard must be a bool expression, got {}",
                        self.type_name(&guard.type_hash)?
                    );
                }
                pattern.insert("guard".to_string(), json!(guard.expr_hash));
            }
            let body = self.type_expr_with_locals(
                current_module,
                &arm.body,
                root,
                param_names,
                param_types,
                region_scope,
                locals,
            )?;
            if fallback_type.is_none() {
                fallback_type = Some(body.type_hash.clone());
            }
            // Early exit (R7): a divergent arm neither fixes nor must match the
            // result type (mirrors the `if` and enum-`case` joins).
            if !crate::expr::raw_expr_diverges(&arm.body) {
                if let Some(expected) = &result_type {
                    if expected != &body.type_hash {
                        bail!(
                            "case arm returns {}, expected {}",
                            self.type_name(&body.type_hash)?,
                            self.type_name(expected)?
                        );
                    }
                } else {
                    result_type = Some(body.type_hash.clone());
                }
            }
            pattern.insert("body".to_string(), json!(body.expr_hash));
            arms_json.push(JsonValue::Object(pattern));
        }
        let exhaustive = has_default
            || match kind {
                ScalarCaseKind::I64 => false,
                ScalarCaseKind::Bool => {
                    seen_bool.contains(&true) && seen_bool.contains(&false)
                }
            };
        if !exhaustive {
            bail!(
                "case expression is not exhaustive: a scalar `case` on {kind} must end with a `_` wildcard{}",
                match kind {
                    ScalarCaseKind::Bool => " (or cover both true and false)",
                    ScalarCaseKind::I64 => "",
                }
            );
        }
        let type_hash = result_type
            .or(fallback_type)
            .ok_or_else(|| anyhow!("case expression has no arms"))?;
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "case",
                "expr": scrutinee.expr_hash,
                "arms": arms_json,
                "type": type_hash,
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": type_hash }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash,
        })
    }

    fn type_field_access(
        &mut self,
        root: &ProgramRootPayload,
        target: &TypeCheckResult,
        field: &str,
    ) -> Result<TypeCheckResult> {
        let field_type = self.field_access_type_in_root(root, &target.type_hash, field)?;
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "field_access",
                "target": target.expr_hash,
                "field": field,
                "type": field_type,
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": field_type }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash: field_type,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn type_builtin_raw_pointer_call(
        &mut self,
        current_module: &str,
        name: &str,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
    ) -> Result<TypeCheckResult> {
        match name {
            "raw_ptr" | "raw_mut_ptr" => {
                if args.len() != 1 {
                    bail!("{name} expects 1 arg, got {}", args.len());
                }
                let value = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let target_mutable = name == "raw_mut_ptr";
                let pointee = match self.type_spec_in_root(root, &value.type_hash)? {
                    TypeSpec::Reference {
                        mutable, referent, ..
                    } => {
                        if target_mutable && !mutable {
                            bail!("raw_mut_ptr expects a mutable reference or raw mutable pointer");
                        }
                        referent
                    }
                    TypeSpec::RawPointer {
                        mutable, pointee, ..
                    } => {
                        if target_mutable && !mutable {
                            bail!("raw_mut_ptr cannot cast a shared raw pointer to mutable");
                        }
                        pointee
                    }
                    other => bail!(
                        "{name} expects a reference or raw pointer, got {}",
                        other.to_source(self)?
                    ),
                };
                let type_hash = self.put_structural_type(TypeSpec::RawPointer {
                    mutable: target_mutable,
                    pointee: pointee.clone(),
                })?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "raw_ptr_cast",
                        "value": value.expr_hash,
                        "source_type": value.type_hash,
                        "pointee_type": pointee,
                        "mutable": target_mutable,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "raw_load" => {
                if args.len() != 1 {
                    bail!("raw_load expects 1 arg, got {}", args.len());
                }
                let pointer = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let TypeSpec::RawPointer { pointee, .. } =
                    self.type_spec_in_root(root, &pointer.type_hash)?
                else {
                    bail!("raw_load expects a raw pointer");
                };
                let class = self.value_class_in_root(root, &pointee)?;
                if class.copy_kind != ValueCopyKind::Copy
                    || class.drop_kind != ValueDropKind::Trivial
                    || class.contains_reference
                {
                    bail!(
                        "raw_load currently supports only Copy, non-reference values with trivial drop"
                    );
                }
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "raw_load",
                        "pointer": pointer.expr_hash,
                        "pointer_type": pointer.type_hash,
                        "pointee_type": pointee.clone(),
                        "type": pointee.clone(),
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": pointee }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: pointee,
                })
            }
            "raw_store" => {
                if args.len() != 2 {
                    bail!("raw_store expects 2 args, got {}", args.len());
                }
                let pointer = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let TypeSpec::RawPointer {
                    mutable: true,
                    pointee,
                } = self.type_spec_in_root(root, &pointer.type_hash)?
                else {
                    bail!("raw_store expects a raw mutable pointer");
                };
                let class = self.value_class_in_root(root, &pointee)?;
                if class.copy_kind != ValueCopyKind::Copy
                    || class.drop_kind != ValueDropKind::Trivial
                    || class.contains_reference
                {
                    bail!(
                        "raw_store currently supports only Copy, non-reference values with trivial drop"
                    );
                }
                let value = self.type_expr_with_locals_expecting(
                    current_module,
                    &args[1],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    Some(&pointee),
                )?;
                if !self.type_assignable_in_root(root, &value.type_hash, &pointee)? {
                    require_type(&value.type_hash, &pointee, "raw_store value", self)?;
                }
                let type_hash = type_hash_for("Unit");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "raw_store",
                        "pointer": pointer.expr_hash,
                        "value": value.expr_hash,
                        "pointer_type": pointer.type_hash,
                        "pointee_type": pointee,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            _ => bail!("unknown raw pointer builtin {name}"),
        }
    }

    fn type_static_bytes_literal(
        &mut self,
        bytes_hex: String,
        literal_kind: &str,
    ) -> Result<TypeCheckResult> {
        validate_hex_bytes(&bytes_hex)?;
        let len = bytes_hex.len() / 2;
        if literal_kind == "string" {
            String::from_utf8(hex_to_bytes(&bytes_hex)?)
                .map_err(|_| anyhow!("string literal bytes must be utf8"))?;
        } else if literal_kind != "bytes" {
            bail!("unknown static literal kind {literal_kind}");
        }

        let data_hash = self.put_object("StaticData", &static_data_payload(&bytes_hex, len)?)?;
        let element_type = type_hash_for("U8");
        let region = static_region_hash();
        let type_hash = self.put_structural_type(TypeSpec::Slice {
            region: region.clone(),
            mutable: false,
            element: element_type.clone(),
        })?;
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "static_bytes",
                "static_data": data_hash,
                "literal_kind": literal_kind,
                "bytes_len": len as u64,
                "region": region,
                "region_name": "static",
                "element_type": element_type,
                "type": type_hash,
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": type_hash }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn type_builtin_box_new(
        &mut self,
        current_module: &str,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
    ) -> Result<TypeCheckResult> {
        if args.len() != 1 {
            bail!("box_new expects 1 arg, got {}", args.len());
        }
        let value = self.type_expr_with_locals(
            current_module,
            &args[0],
            root,
            param_names,
            param_types,
            region_scope,
            locals,
        )?;
        let type_hash = self.put_structural_type(TypeSpec::Box {
            element: value.type_hash.clone(),
        })?;
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "box_new",
                "value": value.expr_hash,
                "element_type": value.type_hash,
                "type": type_hash,
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": type_hash }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash,
        })
    }

    /// `unbox(b: box<T>) -> T`: move the payload out of the heap and free the box
    /// shell. The argument is consumed (the box is move-only), and the result is an
    /// owned `T` (SPEC_V3 §6/Phase 6: the deref-by-move that turns a `box<Node>`
    /// payload back into a `Node` to recurse on).
    #[allow(clippy::too_many_arguments)]
    fn type_builtin_unbox(
        &mut self,
        current_module: &str,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
    ) -> Result<TypeCheckResult> {
        if args.len() != 1 {
            bail!("unbox expects 1 arg, got {}", args.len());
        }
        let value = self.type_expr_with_locals(
            current_module,
            &args[0],
            root,
            param_names,
            param_types,
            region_scope,
            locals,
        )?;
        let element_type = match self.type_spec(&value.type_hash)? {
            TypeSpec::Box { element } => element,
            _ => bail!("unbox expects a box<T> argument, got {}", value.type_hash),
        };
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "unbox",
                "value": value.expr_hash,
                "element_type": element_type.clone(),
                "box_type": value.type_hash,
                "type": element_type.clone(),
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": element_type.clone() }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash: element_type,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn type_builtin_int_cast(
        &mut self,
        current_module: &str,
        target_name: &str,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
    ) -> Result<TypeCheckResult> {
        if args.len() != 1 {
            bail!("to_{} expects 1 arg, got {}", target_name.to_ascii_lowercase(), args.len());
        }
        let value = self.type_expr_with_locals(
            current_module,
            &args[0],
            root,
            param_names,
            param_types,
            region_scope,
            locals,
        )?;
        if scalar_int_type_by_hash(&value.type_hash).is_none() {
            bail!(
                "to_{} expects a sized integer argument, got {}",
                target_name.to_ascii_lowercase(),
                self.type_name(&value.type_hash)?
            );
        }
        let target = type_hash_for(target_name);
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "int_cast",
                "value": value.expr_hash,
                "source_type": value.type_hash,
                "type": target.clone(),
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": target.clone() }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash: target,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn type_builtin_dynamic_buffer_call(
        &mut self,
        current_module: &str,
        name: &str,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
        expected_type: Option<&str>,
    ) -> Result<TypeCheckResult> {
        match name {
            "vec_new" => {
                if args.len() != 1 {
                    bail!("vec_new expects 1 arg, got {}", args.len());
                }
                let expected_type = expected_type.ok_or_else(|| {
                    anyhow!("vec_new requires a destination vec<T> type annotation")
                })?;
                let TypeSpec::Vec { element } = self.type_spec_in_root(root, expected_type)? else {
                    bail!(
                        "vec_new requires a destination vec<T> type annotation, got {}",
                        self.type_name(expected_type)?
                    );
                };
                self.require_phase20_buffer_element(root, &element, "vec_new")?;
                let capacity = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                require_type(
                    &capacity.type_hash,
                    &type_hash_for("I64"),
                    "vec_new capacity",
                    self,
                )?;
                let capacity_value = self
                    .typed_literal_i64_value(&capacity.expr_hash)?
                    .ok_or_else(|| {
                        anyhow!("vec_new capacity must be an i64 literal in phase 20")
                    })?;
                if capacity_value < 0 {
                    bail!("vec_new capacity must be non-negative, got {capacity_value}");
                }
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "vec_new",
                        "capacity": capacity.expr_hash,
                        "capacity_type": capacity.type_hash,
                        "capacity_value": capacity_value as u64,
                        "element_type": element,
                        "type": expected_type,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": expected_type }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: expected_type.to_string(),
                })
            }
            "vec_push" => {
                if args.len() != 2 {
                    bail!("vec_push expects 2 args, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_assignable_place(root, &target.expr_hash)? {
                    bail!("vec_push target must be a mutable vec place");
                }
                let TypeSpec::Vec { element } = self.type_spec_in_root(root, &target.type_hash)?
                else {
                    bail!(
                        "vec_push target must be vec<T>, got {}",
                        self.type_name(&target.type_hash)?
                    );
                };
                self.require_phase20_buffer_element(root, &element, "vec_push")?;
                let value = self.type_expr_with_locals_expecting(
                    current_module,
                    &args[1],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    Some(&element),
                )?;
                if !self.type_assignable_in_root(root, &value.type_hash, &element)? {
                    require_type(&value.type_hash, &element, "vec_push value", self)?;
                }
                let type_hash = type_hash_for("Unit");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "vec_push",
                        "target": target.expr_hash,
                        "value": value.expr_hash,
                        "vec_type": target.type_hash,
                        "element_type": element,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "vec_get" => {
                if args.len() != 2 {
                    bail!("vec_get expects 2 args, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_place(&target.expr_hash)? {
                    bail!("vec_get target must be an addressable vec place");
                }
                let TypeSpec::Vec { element } = self.type_spec_in_root(root, &target.type_hash)?
                else {
                    bail!(
                        "vec_get target must be vec<T>, got {}",
                        self.type_name(&target.type_hash)?
                    );
                };
                self.require_phase20_buffer_element(root, &element, "vec_get")?;
                let index = self.type_expr_with_locals(
                    current_module,
                    &args[1],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                require_type(
                    &index.type_hash,
                    &type_hash_for("I64"),
                    "vec_get index",
                    self,
                )?;
                if let Some(value) = self.typed_literal_i64_value(&index.expr_hash)?
                    && value < 0
                {
                    bail!("vec_get index must be non-negative, got {value}");
                }
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "vec_get",
                        "target": target.expr_hash,
                        "index": index.expr_hash,
                        "vec_type": target.type_hash,
                        "element_type": element,
                        "type": element,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": element }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: element,
                })
            }
            "vec_len" => {
                if args.len() != 1 {
                    bail!("vec_len expects 1 arg, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_place(&target.expr_hash)? {
                    bail!("vec_len target must be an addressable vec place");
                }
                let TypeSpec::Vec { element } = self.type_spec_in_root(root, &target.type_hash)?
                else {
                    bail!(
                        "vec_len target must be vec<T>, got {}",
                        self.type_name(&target.type_hash)?
                    );
                };
                self.require_phase20_buffer_element(root, &element, "vec_len")?;
                let type_hash = type_hash_for("I64");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "vec_len",
                        "target": target.expr_hash,
                        "vec_type": target.type_hash,
                        "element_type": element,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "string_new" => {
                if args.len() != 1 {
                    bail!("string_new expects 1 arg, got {}", args.len());
                }
                let source = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let TypeSpec::Slice {
                    mutable: false,
                    element,
                    ..
                } = self.type_spec_in_root(root, &source.type_hash)?
                else {
                    bail!("string_new source must be an immutable u8 slice");
                };
                if element != type_hash_for("U8") {
                    bail!("string_new source must be an immutable u8 slice");
                }
                let source_payload = self.get_payload(&source.expr_hash)?;
                if source_payload.get("expr_kind").and_then(JsonValue::as_str)
                    != Some("static_bytes")
                {
                    bail!("string_new currently requires a static string or bytes literal source");
                }
                let static_data = source_payload
                    .get("static_data")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_new source missing static_data"))?;
                let bytes_hex = self.static_data_bytes_hex(static_data)?;
                let type_hash = self.put_structural_type(TypeSpec::String)?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "string_new",
                        "source": source.expr_hash,
                        "source_type": source.type_hash,
                        "source_static_data": static_data,
                        "bytes_len": (bytes_hex.len() / 2) as u64,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "string_len" => {
                if args.len() != 1 {
                    bail!("string_len expects 1 arg, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_place(&target.expr_hash)? {
                    bail!("string_len target must be an addressable string place");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target.type_hash)?,
                    TypeSpec::String
                ) {
                    bail!(
                        "string_len target must be string, got {}",
                        self.type_name(&target.type_hash)?
                    );
                }
                let type_hash = type_hash_for("I64");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "string_len",
                        "target": target.expr_hash,
                        "string_type": target.type_hash,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "string_with_capacity" => {
                // `string_with_capacity(n)` allocates an empty (len 0), growable-to-`n`
                // string buffer. Unlike `vec_new`/`string_new` the capacity is a
                // runtime `i64` (not a literal): concat/substring/fmt compute the
                // needed size from runtime lengths. The buffer never reallocs; pushing
                // past `n` traps, exactly like `vec_push`.
                if args.len() != 1 {
                    bail!("string_with_capacity expects 1 arg, got {}", args.len());
                }
                let capacity = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                require_type(
                    &capacity.type_hash,
                    &type_hash_for("I64"),
                    "string_with_capacity capacity",
                    self,
                )?;
                let type_hash = self.put_structural_type(TypeSpec::String)?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "string_with_capacity",
                        "capacity": capacity.expr_hash,
                        "capacity_type": capacity.type_hash,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "string_push" => {
                // `string_push(s, b)` appends byte `b` to string place `s` (no realloc;
                // traps at capacity). Mirrors `vec_push` over a `u8` element.
                if args.len() != 2 {
                    bail!("string_push expects 2 args, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_assignable_place(root, &target.expr_hash)? {
                    bail!("string_push target must be a mutable string place");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target.type_hash)?,
                    TypeSpec::String
                ) {
                    bail!(
                        "string_push target must be string, got {}",
                        self.type_name(&target.type_hash)?
                    );
                }
                let value = self.type_expr_with_locals_expecting(
                    current_module,
                    &args[1],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    Some(&type_hash_for("U8")),
                )?;
                require_type(
                    &value.type_hash,
                    &type_hash_for("U8"),
                    "string_push value",
                    self,
                )?;
                let type_hash = type_hash_for("Unit");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "string_push",
                        "target": target.expr_hash,
                        "value": value.expr_hash,
                        "string_type": target.type_hash,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "string_get" => {
                // `string_get(s, i)` reads byte `i` of string place `s` (bounds-checked
                // against `len`). Mirrors `vec_get`; result is a `u8` copy.
                if args.len() != 2 {
                    bail!("string_get expects 2 args, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_place(&target.expr_hash)? {
                    bail!("string_get target must be an addressable string place");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target.type_hash)?,
                    TypeSpec::String
                ) {
                    bail!(
                        "string_get target must be string, got {}",
                        self.type_name(&target.type_hash)?
                    );
                }
                let index = self.type_expr_with_locals(
                    current_module,
                    &args[1],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                require_type(
                    &index.type_hash,
                    &type_hash_for("I64"),
                    "string_get index",
                    self,
                )?;
                if let Some(value) = self.typed_literal_i64_value(&index.expr_hash)?
                    && value < 0
                {
                    bail!("string_get index must be non-negative, got {value}");
                }
                let type_hash = type_hash_for("U8");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "string_get",
                        "target": target.expr_hash,
                        "index": index.expr_hash,
                        "string_type": target.type_hash,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "string_set" => {
                // `string_set(s, i, b)` overwrites byte `i` of string place `s`
                // (bounds-checked against `len`) — the random-access write twin of
                // `string_get`, with `string_push`'s mutable-place discipline.
                if args.len() != 3 {
                    bail!("string_set expects 3 args, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_assignable_place(root, &target.expr_hash)? {
                    bail!("string_set target must be a mutable string place");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target.type_hash)?,
                    TypeSpec::String
                ) {
                    bail!(
                        "string_set target must be string, got {}",
                        self.type_name(&target.type_hash)?
                    );
                }
                let index = self.type_expr_with_locals(
                    current_module,
                    &args[1],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                require_type(
                    &index.type_hash,
                    &type_hash_for("I64"),
                    "string_set index",
                    self,
                )?;
                if let Some(value) = self.typed_literal_i64_value(&index.expr_hash)?
                    && value < 0
                {
                    bail!("string_set index must be non-negative, got {value}");
                }
                let value = self.type_expr_with_locals_expecting(
                    current_module,
                    &args[2],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                    Some(&type_hash_for("U8")),
                )?;
                require_type(
                    &value.type_hash,
                    &type_hash_for("U8"),
                    "string_set value",
                    self,
                )?;
                let type_hash = type_hash_for("Unit");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "string_set",
                        "target": target.expr_hash,
                        "index": index.expr_hash,
                        "value": value.expr_hash,
                        "string_type": target.type_hash,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            _ => bail!("unknown dynamic buffer builtin {name}"),
        }
    }

    fn require_phase20_buffer_element(
        &self,
        root: &ProgramRootPayload,
        element_type: &str,
        op: &str,
    ) -> Result<()> {
        let class = self.value_class_in_root(root, element_type)?;
        if class.copy_kind != ValueCopyKind::Copy
            || class.drop_kind != ValueDropKind::Trivial
            || class.contains_reference
        {
            bail!("{op} currently supports only Copy, non-reference elements with trivial drop");
        }
        let layout = self.compute_type_layout(root, element_type, DEFAULT_NATIVE_TARGET)?;
        let size = layout
            .metadata
            .get("size_bytes")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("{op} element layout missing size_bytes"))?;
        if !matches!(size, 1 | 8) {
            bail!("{op} currently supports element sizes 1 and 8 bytes, got {size}");
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn type_builtin_slice_call(
        &mut self,
        current_module: &str,
        name: &str,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
    ) -> Result<TypeCheckResult> {
        match name {
            "slice" | "mut_slice" => {
                if args.len() != 1 {
                    bail!("{name} expects 1 arg, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.typed_expr_is_place(&target.expr_hash)? {
                    bail!("{name} target must be an addressable array place");
                }
                let mutable = name == "mut_slice";
                if mutable && !self.typed_expr_is_assignable_place(root, &target.expr_hash)? {
                    bail!("mut_slice target must be a mutable semantic place");
                }
                let (region_name, region_hash) =
                    self.resolve_single_region_for_builtin(region_scope, name)?;
                // Fail closed on a reference target. `indexed_element_type_in_root`
                // auto-derefs references (correct for indexing, which lowers a
                // deref), but `lower_slice_from_array` uses the target type
                // directly, so `slice(&array)` would type-check yet have no native
                // layout (no element_type_hash) and fail only at build. Reject it
                // here like `subslice`/`len` do, so verify is a sound native gate.
                if let TypeSpec::Reference { .. } =
                    self.type_spec_in_root(root, &target.type_hash)?
                {
                    bail!("{name} target must be a fixed array, not a reference");
                }
                let info = self.indexed_element_type_in_root(root, &target.type_hash)?;
                let IndexedElementInfo::FixedArray {
                    container_type,
                    element_type,
                    len,
                } = info
                else {
                    bail!("{name} target must be a fixed array");
                };
                let type_hash = self.put_structural_type(TypeSpec::Slice {
                    region: region_hash.clone(),
                    mutable,
                    element: element_type.clone(),
                })?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "slice_from_array",
                        "target": target.expr_hash,
                        "target_type": target.type_hash,
                        "array_type": container_type,
                        "element_type": element_type,
                        "len": len,
                        "region": region_hash,
                        "region_name": region_name,
                        "mutable": mutable,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "len" => {
                if args.len() != 1 {
                    bail!("len expects 1 arg, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let TypeSpec::Slice { .. } = self.type_spec_in_root(root, &target.type_hash)?
                else {
                    bail!("len expects a slice");
                };
                let type_hash = type_hash_for("I64");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "slice_len",
                        "target": target.expr_hash,
                        "slice_type": target.type_hash,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            "subslice" => {
                if args.len() != 3 {
                    bail!("subslice expects 3 args, got {}", args.len());
                }
                let target = self.type_expr_with_locals(
                    current_module,
                    &args[0],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let TypeSpec::Slice { element, .. } =
                    self.type_spec_in_root(root, &target.type_hash)?
                else {
                    bail!("subslice expects a slice");
                };
                let start = self.type_expr_with_locals(
                    current_module,
                    &args[1],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let len = self.type_expr_with_locals(
                    current_module,
                    &args[2],
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                require_type(
                    &start.type_hash,
                    &type_hash_for("I64"),
                    "subslice start",
                    self,
                )?;
                require_type(&len.type_hash, &type_hash_for("I64"), "subslice len", self)?;
                if matches!(self.typed_literal_i64_value(&start.expr_hash)?, Some(value) if value < 0)
                {
                    bail!("subslice start must be non-negative");
                }
                if matches!(self.typed_literal_i64_value(&len.expr_hash)?, Some(value) if value < 0)
                {
                    bail!("subslice len must be non-negative");
                }
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "subslice",
                        "target": target.expr_hash,
                        "start": start.expr_hash,
                        "len": len.expr_hash,
                        "slice_type": target.type_hash,
                        "element_type": element,
                        "type": target.type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": target.type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: target.type_hash,
                })
            }
            _ => bail!("unknown slice builtin {name}"),
        }
    }

    fn resolve_single_region_for_builtin(
        &self,
        region_scope: &BTreeMap<String, String>,
        name: &str,
    ) -> Result<(String, String)> {
        if region_scope.len() != 1 {
            bail!(
                "{name} requires exactly one function region parameter; found {}",
                region_scope.len()
            );
        }
        let (name, hash) = region_scope
            .iter()
            .next()
            .expect("region_scope length was checked");
        Ok((name.clone(), hash.clone()))
    }

    /// `array_set(arr, i, v)` (R9): functional update of one element of a fixed
    /// array — yields a NEW `array<T, N>` equal to `arr` with element `i` set to
    /// `v`. Like `[value; count]`, the element type must be a non-reference Copy
    /// value with trivial drop: the whole array is Copy (so a `loop` can carry it
    /// and the update needs no move/loan bookkeeping), and overwriting element `i`
    /// silently discards the old element (sound only when it needs no drop). `i`
    /// is bounds-checked at runtime against `N` (a literal out-of-range `i` is
    /// rejected here). This is the array counterpart of `string_push`/`vec_push`
    /// for a Copy fixed buffer, and the substrate for a worklist that builds an
    /// array by index (e.g. the SHA-256 message schedule).
    #[allow(clippy::too_many_arguments)]
    /// Type the process-argument builtins (R12): `arg_count() -> i64`,
    /// `arg_len(i: i64) -> i64`, `arg_byte(i: i64, j: i64) -> u8`. They read
    /// the process's command-line arguments (the program name excluded), an
    /// ambient input — so they require the `io` effect. Out-of-range indices
    /// are a runtime error (eval) / trap (native), like other bounds checks.
    #[allow(clippy::too_many_arguments)]
    fn type_builtin_process_arg(
        &mut self,
        current_module: &str,
        name: &str,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
    ) -> Result<TypeCheckResult> {
        let (arity, keys, result): (usize, &[&str], &str) = match name {
            "arg_count" => (0, &[], "I64"),
            "arg_len" => (1, &["index"], "I64"),
            "arg_byte" => (2, &["index", "byte"], "U8"),
            other => bail!("unknown process-argument builtin {other}"),
        };
        if args.len() != arity {
            bail!("{name} expects {arity} args, got {}", args.len());
        }
        let i64_hash = type_hash_for("I64");
        let mut payload = serde_json::Map::new();
        payload.insert("expr_kind".to_string(), json!(name));
        for (arg, key) in args.iter().zip(keys) {
            let typed = self.type_expr_with_locals_expecting(
                current_module,
                arg,
                root,
                param_names,
                param_types,
                region_scope,
                locals,
                Some(&i64_hash),
            )?;
            if typed.type_hash != i64_hash {
                bail!(
                    "{name} {key} must be i64, got {}",
                    self.type_name(&typed.type_hash)?
                );
            }
            payload.insert(key.to_string(), json!(typed.expr_hash));
        }
        let type_hash = type_hash_for(result);
        payload.insert("type".to_string(), json!(type_hash));
        let expr_hash = self.put_object("Expression", &JsonValue::Object(payload))?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": type_hash }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn type_builtin_array_set(
        &mut self,
        current_module: &str,
        args: &[RawExpr],
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
        region_scope: &BTreeMap<String, String>,
        locals: &mut Vec<LocalTypeBinding>,
        expected_type: Option<&str>,
    ) -> Result<TypeCheckResult> {
        if args.len() != 3 {
            bail!("array_set expects 3 args (array, index, value), got {}", args.len());
        }
        // `array_set` returns the same array type as its first argument, so an
        // expected result type (e.g. a `let`-binding annotation `array<u32, N>`)
        // anchors that argument — letting a bare `array_set([0x0; N], ..)` build a
        // sized-element array. A nested `array_set` propagates the same expectation.
        let array = self.type_expr_with_locals_expecting(
            current_module,
            &args[0],
            root,
            param_names,
            param_types,
            region_scope,
            locals,
            expected_type,
        )?;
        let TypeSpec::FixedArray { element, len } = self.type_spec_in_root(root, &array.type_hash)?
        else {
            bail!(
                "array_set target must be a fixed array, got {}",
                self.type_name(&array.type_hash)?
            );
        };
        // The result array is Copy and element `i` is overwritten without dropping
        // the old value, so the element must be a non-reference Copy value with
        // trivial drop (the array-fill discipline) — keeping the borrow/move
        // analyses trivial and the overwrite leak-free.
        let class = self.value_class_in_root(root, &element)?;
        if class.copy_kind != ValueCopyKind::Copy
            || class.drop_kind != ValueDropKind::Trivial
            || class.contains_reference
        {
            bail!(
                "array_set element must be a non-reference Copy value with trivial drop \
                 (the array is copied and element i overwritten in place), got {}",
                self.type_name(&element)?
            );
        }
        let index = self.type_expr_with_locals(
            current_module,
            &args[1],
            root,
            param_names,
            param_types,
            region_scope,
            locals,
        )?;
        require_type(&index.type_hash, &type_hash_for("I64"), "array_set index", self)?;
        if let Some(value) = self.typed_literal_i64_value(&index.expr_hash)?
            && (value < 0 || value as u64 >= len)
        {
            bail!("array_set index {value} out of bounds for length {len}");
        }
        let value = self.type_expr_with_locals_expecting(
            current_module,
            &args[2],
            root,
            param_names,
            param_types,
            region_scope,
            locals,
            Some(&element),
        )?;
        if !self.type_assignable_in_root(root, &value.type_hash, &element)? {
            bail!(
                "array_set value type {} does not match element type {}",
                self.type_name(&value.type_hash)?,
                self.type_name(&element)?
            );
        }
        let type_hash = array.type_hash.clone();
        let expr_hash = self.put_object(
            "Expression",
            &json!({
                "expr_kind": "array_set",
                "array": array.expr_hash,
                "index": index.expr_hash,
                "value": value.expr_hash,
                "element_type": element,
                "type": type_hash.clone(),
            }),
        )?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": type_hash }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash,
        })
    }

    fn type_array_index(
        &mut self,
        root: &ProgramRootPayload,
        target: &TypeCheckResult,
        index: &TypeCheckResult,
    ) -> Result<TypeCheckResult> {
        require_type(&index.type_hash, &type_hash_for("I64"), "array index", self)?;
        let info = self.indexed_element_type_in_root(root, &target.type_hash)?;
        let mut payload = json!({
                "expr_kind": "array_index",
                "target": target.expr_hash.clone(),
                "index": index.expr_hash.clone(),
                "target_type": target.type_hash.clone(),
        });
        let element_type = match info {
            IndexedElementInfo::FixedArray {
                container_type,
                element_type,
                len,
            } => {
                if let Some(value) = self.typed_literal_i64_value(&index.expr_hash)?
                    && (value < 0 || value as u64 >= len)
                {
                    bail!("array index {value} out of bounds for length {len}");
                }
                payload["indexed_kind"] = JsonValue::String("fixed_array".to_string());
                payload["array_type"] = JsonValue::String(container_type);
                payload["element_type"] = JsonValue::String(element_type.clone());
                payload["len"] = JsonValue::from(len);
                element_type
            }
            IndexedElementInfo::Slice {
                container_type,
                element_type,
            } => {
                if let Some(value) = self.typed_literal_i64_value(&index.expr_hash)?
                    && value < 0
                {
                    bail!("slice index must be non-negative, got {value}");
                }
                payload["indexed_kind"] = JsonValue::String("slice".to_string());
                payload["slice_type"] = JsonValue::String(container_type);
                payload["element_type"] = JsonValue::String(element_type.clone());
                element_type
            }
        };
        payload["type"] = JsonValue::String(element_type.clone());
        let expr_hash = self.put_object("Expression", &payload)?;
        self.write_cache_json(
            &expr_hash,
            "typechecker",
            "typed-dag",
            ArtifactKind::TypedExpression,
            &json!({ "type": element_type }),
        )?;
        Ok(TypeCheckResult {
            expr_hash,
            type_hash: element_type,
        })
    }

    fn indexed_element_type_in_root(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<IndexedElementInfo> {
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Reference { referent, .. } => {
                self.indexed_element_type_in_root(root, &referent)
            }
            TypeSpec::Slice { element, .. } => Ok(IndexedElementInfo::Slice {
                container_type: type_hash.to_string(),
                element_type: element,
            }),
            TypeSpec::FixedArray { element, len } => Ok(IndexedElementInfo::FixedArray {
                container_type: type_hash.to_string(),
                element_type: element,
                len,
            }),
            other => bail!(
                "index requires array or slice type, got {}",
                other.to_source(self)?
            ),
        }
    }

    fn typed_literal_i64_value(&self, expr_hash: &str) -> Result<Option<i64>> {
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

    fn array_index_place_segment(&self, index_hash: &str) -> Result<String> {
        Ok(match self.typed_literal_i64_value(index_hash)? {
            Some(value) if value >= 0 => array_index_segment(value as u64),
            _ => "[*]".to_string(),
        })
    }

    fn typed_expr_is_place(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        Ok(matches!(
            payload.get("expr_kind").and_then(JsonValue::as_str),
            Some("param_ref" | "local_ref" | "field_access" | "array_index")
        ))
    }

    fn typed_expr_is_assignable_place(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
    ) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "param_ref" | "local_ref" => Ok(true),
            "field_access" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                let target_payload = self.get_payload(target_hash)?;
                let target_type = target_payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access target missing type"))?;
                match self.type_spec_in_root(root, target_type)? {
                    TypeSpec::Slice { mutable, .. } => Ok(mutable),
                    TypeSpec::Reference { mutable, .. } => Ok(mutable),
                    _ => self.typed_expr_is_assignable_place(root, target_hash),
                }
            }
            "array_index" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let target_payload = self.get_payload(target_hash)?;
                let target_type = target_payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index target missing type"))?;
                match self.type_spec_in_root(root, target_type)? {
                    TypeSpec::Slice { mutable, .. } => Ok(mutable),
                    TypeSpec::Reference { mutable, .. } => Ok(mutable),
                    _ => self.typed_expr_is_assignable_place(root, target_hash),
                }
            }
            _ => Ok(false),
        }
    }

    pub(crate) fn type_check_root(&self, root_hash: &str) -> Result<()> {
        let root = self.load_root(root_hash)?;
        self.validate_root_type_definitions(&root)?;
        for entry in &root.symbols {
            let (param_types, return_type) = self.signature_parts(&entry.signature)?;
            let region_params = self.signature_region_params(&entry.signature)?;
            validate_region_params(&region_params)?;
            let allowed_regions = region_params
                .iter()
                .map(|param| param.region.clone())
                .collect::<BTreeSet<_>>();
            // Generic functions (R11): a `TypeParam { index }` in the signature is
            // valid only for `index < type_param_count`. The template is checked
            // once with its parameters opaque; its concrete instances are ordinary
            // functions (no type parameters) checked normally.
            let type_param_count = self.signature_type_params(&entry.signature)?.len();
            self.signature_effects(&entry.signature)?;
            for param_type in &param_types {
                self.validate_type_hash_in_root_with_params(
                    &root,
                    param_type,
                    &allowed_regions,
                    type_param_count,
                )?;
            }
            self.validate_type_hash_in_root_with_params(
                &root,
                &return_type,
                &allowed_regions,
                type_param_count,
            )?;
            let definition_signature = self.function_signature_hash(&entry.definition)?;
            if definition_signature != entry.signature {
                bail!(
                    "bad_signature: root signature {} does not match definition signature {}",
                    entry.signature,
                    definition_signature
                );
            }
            if self.definition_is_external(&entry.definition)? {
                let external = self.external_function_metadata(&entry.definition)?;
                if external.symbol != entry.symbol {
                    bail!("bad_external: external function symbol does not match root symbol");
                }
                if external.signature != entry.signature {
                    bail!("bad_external: external function signature does not match root");
                }
                self.validate_external_signature_effects(Some(&root), &entry.signature)
                    .context("bad_external: external function signature effects are invalid")?;
                continue;
            }
            let body = self.function_body_hash(&entry.definition)?;
            let actual = self.verify_expr_type(
                &body,
                &root,
                &param_types,
                &allowed_regions,
                Some(&return_type),
            )?;
            if !self.type_assignable_in_root(&root, &actual, &return_type)? {
                bail!(
                    "bad_type: function {} returns {}, body is {}",
                    self.symbol_display(&root, &entry.symbol)?,
                    self.type_name(&return_type)?,
                    self.type_name(&actual)?
                );
            }
            if self.expr_escapes_local_borrow(&root, &body, &mut Vec::new())? {
                bail!(
                    "bad_borrow: function {} returns reference to local storage",
                    self.symbol_display(&root, &entry.symbol)?
                );
            }
            self.verify_function_borrows(&root, entry, &param_types)?;
            self.verify_function_effects(&root, entry)?;
        }
        self.validate_tests_for_root(root_hash, &root)?;
        Ok(())
    }

    fn expr_escapes_local_borrow(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        locals_with_local_borrows: &mut Vec<bool>,
    ) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        match expr_kind {
            "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" | "param_ref" => {
                Ok(false)
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                Ok(local_bool_at_depth(locals_with_local_borrows, depth).unwrap_or(false))
            }
            "borrow_shared" | "borrow_mut" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow expression missing target"))?;
                self.borrow_target_is_local_storage(root, target, locals_with_local_borrows)
            }
            "slice_from_array" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_from_array missing target"))?;
                self.borrow_target_is_local_storage(root, target, locals_with_local_borrows)
            }
            "slice_len" => Ok(false),
            "subslice" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing target"))?;
                self.expr_escapes_local_borrow(root, target, locals_with_local_borrows)
            }
            "box_new" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("box_new missing value"))?;
                self.expr_escapes_local_borrow(root, value, locals_with_local_borrows)
            }
            "unbox" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unbox missing value"))?;
                self.expr_escapes_local_borrow(root, value, locals_with_local_borrows)
            }
            "vec_new" | "vec_push" | "vec_get" | "vec_len" | "string_new" | "string_len"
            | "string_with_capacity" | "string_push" | "string_get" | "string_set"
            | "arg_count" | "arg_len" | "arg_byte" => Ok(false),
            "raw_ptr_cast" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?;
                self.expr_escapes_local_borrow(root, value, locals_with_local_borrows)
            }
            "raw_load" => Ok(false),
            "raw_store" => Ok(false),
            "assign" => Ok(false),
            "let" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing value"))?;
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing body"))?;
                let value_has_local_borrow =
                    self.expr_escapes_local_borrow(root, value_hash, locals_with_local_borrows)?;
                locals_with_local_borrows.push(value_has_local_borrow);
                let body_result =
                    self.expr_escapes_local_borrow(root, body_hash, locals_with_local_borrows);
                locals_with_local_borrows.pop();
                body_result
            }
            "record_literal" => {
                for field in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                {
                    let value_hash = field
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing value"))?;
                    if self.expr_escapes_local_borrow(
                        root,
                        value_hash,
                        locals_with_local_borrows,
                    )? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            "array_literal" => {
                for element in payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                {
                    let value_hash = element
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing value"))?;
                    if self.expr_escapes_local_borrow(
                        root,
                        value_hash,
                        locals_with_local_borrows,
                    )? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            "array_fill" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                self.expr_escapes_local_borrow(root, value_hash, locals_with_local_borrows)
            }
            // `array_set` yields a by-value, non-reference Copy array (the element
            // type rule forbids references), so it can never be — or hold — a
            // borrow into local storage.
            "array_set" => Ok(false),
            "field_access" => {
                let declared_type = payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing type"))?;
                if !self.expr_type_can_escape_borrow(root, declared_type)? {
                    return Ok(false);
                }
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                self.expr_escapes_local_borrow(root, target, locals_with_local_borrows)
            }
            "array_index" => {
                let declared_type = payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing type"))?;
                if !self.expr_type_can_escape_borrow(root, declared_type)? {
                    return Ok(false);
                }
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                self.expr_escapes_local_borrow(root, target, locals_with_local_borrows)
            }
            "if" => {
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                Ok(
                    self.expr_escapes_local_borrow(root, then_hash, locals_with_local_borrows)?
                        || self.expr_escapes_local_borrow(
                            root,
                            else_hash,
                            locals_with_local_borrows,
                        )?,
                )
            }
            "return" => {
                // `return <value>` (R7) makes `<value>` a function return value, so
                // a local borrow it carries escapes exactly as a tail return value
                // would. Propagate the operand's escape status.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("return missing value"))?;
                self.expr_escapes_local_borrow(root, value, locals_with_local_borrows)
            }
            "fold" => Ok(false),
            // A `loop` accumulator is copyable and reference-free (R8 restriction),
            // so the loop result cannot carry a borrow of a local.
            "loop" => Ok(false),
            "case" => {
                let expr = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                let scrutinee_has_local_borrow =
                    self.expr_escapes_local_borrow(root, expr, locals_with_local_borrows)?;
                for arm in payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                {
                    let body_hash = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    // A nested pattern's leaf binding (R14) reaches through the
                    // scrutinee's payload, so it inherits the scrutinee's local-borrow
                    // status exactly like a simple binding.
                    let bound = crate::expr::typed_arm_binding_name(arm).is_some();
                    if bound {
                        locals_with_local_borrows.push(scrutinee_has_local_borrow);
                    }
                    let body_result =
                        self.expr_escapes_local_borrow(root, body_hash, locals_with_local_borrows);
                    if bound {
                        locals_with_local_borrows.pop();
                    }
                    if body_result? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            "call" => {
                let (args, return_type, _) = self.call_region_context(root, &payload)?;
                let return_regions = self.reference_regions_in_type(root, &return_type)?;
                let return_has_raw =
                    self.type_contains_raw_pointer(Some(root), &return_type, &mut BTreeSet::new())?;
                if return_regions.is_empty() && !return_has_raw {
                    return Ok(false);
                }
                for arg in args {
                    if !self.expr_escapes_local_borrow(root, &arg, locals_with_local_borrows)? {
                        continue;
                    }
                    let arg_type = self.expr_declared_type(&arg)?;
                    let arg_regions = self.reference_regions_in_type(root, &arg_type)?;
                    if !arg_regions.is_disjoint(&return_regions) {
                        return Ok(true);
                    }
                    // A raw-pointer return is region-erased, so an escaping
                    // raw-pointer argument may flow out through it. Treat that as
                    // an escape rather than letting the empty region set wave it
                    // through (the raw-pointer laundering hole): the direct
                    // `return raw_mut_ptr(&'a mut local[0])` case is already
                    // rejected, and laundering it through a call must be too.
                    if return_has_raw
                        && self.type_contains_raw_pointer(
                            Some(root),
                            &arg_type,
                            &mut BTreeSet::new(),
                        )?
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            "enum_construct" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                self.expr_escapes_local_borrow(root, value, locals_with_local_borrows)
            }
            "binary" | "unary" | "int_cast" => Ok(false),
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn expr_type_can_escape_borrow(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<bool> {
        Ok(self
            .value_class_in_root(root, type_hash)?
            .contains_reference)
    }

    fn borrow_target_is_local_storage(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        locals_with_local_borrows: &mut Vec<bool>,
    ) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        match expr_kind {
            "param_ref" | "local_ref" => Ok(true),
            "field_access" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                let target_type = self.expr_declared_type(target)?;
                if matches!(
                    self.type_spec_in_root(root, &target_type)?,
                    TypeSpec::Reference { .. }
                ) {
                    // The borrow target is `(*r).field`, i.e. a reborrow through
                    // the reference value `target`. The storage it names is local
                    // exactly when `target` is itself a reference into local
                    // storage (e.g. `let r = &local`). Resolve that by asking the
                    // escape analysis whether the reference value carries a borrow
                    // of a local; a reference *parameter* points to caller storage
                    // and is correctly not local.
                    self.expr_escapes_local_borrow(root, target, locals_with_local_borrows)
                } else {
                    self.borrow_target_is_local_storage(root, target, locals_with_local_borrows)
                }
            }
            "array_index" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let target_type = self.expr_declared_type(target)?;
                if matches!(
                    self.type_spec_in_root(root, &target_type)?,
                    TypeSpec::Reference { .. }
                ) {
                    self.expr_escapes_local_borrow(root, target, locals_with_local_borrows)
                } else {
                    self.borrow_target_is_local_storage(root, target, locals_with_local_borrows)
                }
            }
            _ => Ok(false),
        }
    }

    fn call_region_context(
        &self,
        root: &ProgramRootPayload,
        payload: &JsonValue,
    ) -> Result<(Vec<String>, String, BTreeMap<String, String>)> {
        let symbol = payload
            .get("symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("call missing symbol"))?;
        let callee = self
            .root_symbol(root, symbol)
            .ok_or_else(|| anyhow!("call target missing from root {symbol}"))?;
        let (expected_params, return_type) =
            self.call_signature_with_type_args(&callee.signature, payload)?;
        let args = payload
            .get("args")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("call missing args"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("call arg must be hash"))
            })
            .collect::<Result<Vec<_>>>()?;
        if args.len() != expected_params.len() {
            bail!("call arity mismatch for {symbol}");
        }
        let callee_regions = self
            .signature_region_params(&callee.signature)?
            .into_iter()
            .map(|param| param.region)
            .collect::<BTreeSet<_>>();
        let mut region_substitutions = BTreeMap::new();
        for (idx, arg) in args.iter().enumerate() {
            let actual = self.expr_declared_type(arg)?;
            if !self.type_assignable_for_call_in_root(
                root,
                &actual,
                &expected_params[idx],
                &callee_regions,
            )? {
                bail!("call arg type mismatch for {symbol} at arg {idx}");
            }
            self.infer_call_region_substitutions(
                root,
                &actual,
                &expected_params[idx],
                &callee_regions,
                &mut region_substitutions,
            )?;
        }
        let return_type = self.substitute_type_regions_hash(&return_type, &region_substitutions)?;
        Ok((args, return_type, region_substitutions))
    }

    fn reference_regions_in_type(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<BTreeSet<String>> {
        let mut regions = BTreeSet::new();
        let mut seen = BTreeSet::new();
        self.collect_reference_regions_in_type(root, type_hash, &mut regions, &mut seen)?;
        Ok(regions)
    }

    fn collect_reference_regions_in_type(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        regions: &mut BTreeSet<String>,
        seen: &mut BTreeSet<String>,
    ) -> Result<()> {
        // A recursive type (e.g. `enum Node { next: box<Node> }`) reaches itself
        // through a `box`/`vec` element, so guard against revisiting a type — the
        // set of reference regions is finite and a repeat visit adds nothing.
        if !seen.insert(type_hash.to_string()) {
            return Ok(());
        }
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Builtin(_) => Ok(()),
            // `type_spec_in_root` expands named types into their structural
            // Record/Enum form (with regions substituted), so this arm is not
            // reached today; the expanded fields below carry the reference
            // regions. Collect the instantiated region arguments anyway so the
            // function stays sound (rather than silently dropping regions) if the
            // resolver is ever switched to a non-expanding form — every reference
            // region inside a named type is bound to one of its region
            // parameters and so appears in `region_args`.
            TypeSpec::Named { region_args, .. } => {
                for region in region_args {
                    regions.insert(region);
                }
                Ok(())
            }
            TypeSpec::Reference {
                region, referent, ..
            } => {
                regions.insert(region);
                self.collect_reference_regions_in_type(root, &referent, regions, seen)
            }
            TypeSpec::RawPointer { pointee, .. } => {
                self.collect_reference_regions_in_type(root, &pointee, regions, seen)
            }
            TypeSpec::Box { element } => {
                self.collect_reference_regions_in_type(root, &element, regions, seen)
            }
            TypeSpec::Vec { element } => {
                self.collect_reference_regions_in_type(root, &element, regions, seen)
            }
            TypeSpec::String => Ok(()),
            TypeSpec::Slice {
                region, element, ..
            } => {
                regions.insert(region);
                self.collect_reference_regions_in_type(root, &element, regions, seen)
            }
            TypeSpec::FixedArray { element, .. } => {
                self.collect_reference_regions_in_type(root, &element, regions, seen)
            }
            TypeSpec::Record(fields) | TypeSpec::Enum(fields) => {
                for field in fields {
                    self.collect_reference_regions_in_type(root, &field.type_hash, regions, seen)?;
                }
                Ok(())
            }
            // A type parameter is opaque and carries no regions (R11); the
            // concrete instance's regions appear once it is substituted.
            TypeSpec::TypeParam { .. } => Ok(()),
        }
    }

    fn validate_root_type_definitions(&self, root: &ProgramRootPayload) -> Result<()> {
        for entry in &root.types {
            let definition = self.type_definition(&entry.type_def)?;
            if definition.type_symbol() != entry.type_symbol {
                bail!("bad_type_def: root type symbol does not match TypeDef");
            }
            let allowed_regions = definition
                .region_params()
                .iter()
                .map(|param| param.region.clone())
                .collect::<BTreeSet<_>>();
            // A generic type's members may reference its own type parameters
            // (R11), so they are validated with the parameter count in scope.
            let type_param_count = definition.type_params().len();
            let members = match &definition {
                TypeDefinition::Record { fields, .. } => fields,
                TypeDefinition::Enum { variants, .. } => variants,
            };
            for member in members {
                self.validate_type_hash_in_root_with_params(
                    root,
                    &member.type_hash,
                    &allowed_regions,
                    type_param_count,
                )?;
            }
        }
        Ok(())
    }

    fn validate_type_hash_in_root(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        allowed_regions: &BTreeSet<String>,
    ) -> Result<()> {
        self.validate_type_hash_in_root_with_params(root, type_hash, allowed_regions, 0)
    }

    /// Validate that a type hash references only types/regions that exist in scope.
    /// `type_param_count` is the number of type parameters in scope (R11): a
    /// `TypeSpec::TypeParam { index }` is valid only when `index < type_param_count`
    /// (i.e. inside a generic template); it is `0` for ordinary concrete contexts,
    /// where any `TypeParam` is rejected as an escaped parameter.
    fn validate_type_hash_in_root_with_params(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        allowed_regions: &BTreeSet<String>,
        type_param_count: usize,
    ) -> Result<()> {
        let recurse = |this: &Self, inner: &str| {
            this.validate_type_hash_in_root_with_params(
                root,
                inner,
                allowed_regions,
                type_param_count,
            )
        };
        match self.type_spec(type_hash)? {
            TypeSpec::Builtin(_) => Ok(()),
            TypeSpec::TypeParam { index } => {
                if (index as usize) < type_param_count {
                    Ok(())
                } else {
                    bail!("type parameter index {index} out of scope")
                }
            }
            TypeSpec::Named {
                type_symbol,
                region_args,
                type_args,
            } => {
                let entry = self
                    .root_type(root, &type_symbol)
                    .ok_or_else(|| anyhow!("named type missing from root {type_symbol}"))?;
                let definition = self.type_definition(&entry.type_def)?;
                if definition.region_params().len() != region_args.len() {
                    bail!(
                        "named type {} expects {} region args, got {}",
                        type_symbol,
                        definition.region_params().len(),
                        region_args.len()
                    );
                }
                if definition.type_params().len() != type_args.len() {
                    bail!(
                        "named type {} expects {} type args, got {}",
                        type_symbol,
                        definition.type_params().len(),
                        type_args.len()
                    );
                }
                for region in region_args {
                    if !allowed_regions.contains(&region) && !is_static_region(&region) {
                        bail!("invalid region reference {region}");
                    }
                }
                for arg in type_args {
                    recurse(self, &arg)?;
                }
                Ok(())
            }
            TypeSpec::Reference {
                region, referent, ..
            } => {
                if !allowed_regions.contains(&region) && !is_static_region(&region) {
                    bail!("invalid region reference {region}");
                }
                recurse(self, &referent)
            }
            TypeSpec::RawPointer { pointee, .. } => recurse(self, &pointee),
            TypeSpec::Box { element } => recurse(self, &element),
            TypeSpec::Vec { element } => recurse(self, &element),
            TypeSpec::String => Ok(()),
            TypeSpec::Slice {
                region, element, ..
            } => {
                if !allowed_regions.contains(&region) && !is_static_region(&region) {
                    bail!("invalid region reference {region}");
                }
                recurse(self, &element)
            }
            TypeSpec::FixedArray { element, len } => {
                if len > MAX_FIXED_ARRAY_LEN {
                    bail!(
                        "fixed array length {len} exceeds the supported maximum {MAX_FIXED_ARRAY_LEN}"
                    );
                }
                recurse(self, &element)
            }
            TypeSpec::Record(fields) | TypeSpec::Enum(fields) => {
                for field in fields {
                    recurse(self, &field.type_hash)?;
                }
                Ok(())
            }
        }
    }

    fn verify_function_effects(
        &self,
        root: &ProgramRootPayload,
        entry: &crate::model::RootSymbolPayload,
    ) -> Result<()> {
        if self.definition_is_external(&entry.definition)? {
            return Ok(());
        }
        let declared = self
            .signature_effects(&entry.signature)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let body = self.function_body_hash(&entry.definition)?;
        if self.expr_requires_state(&body)? && !declared.contains(&Effect::State) {
            bail!(
                "bad_effects: function {} requires undeclared effect state",
                self.symbol_display(root, &entry.symbol)?
            );
        }
        if self.expr_requires_alloc(&body)? && !declared.contains(&Effect::Alloc) {
            bail!(
                "bad_effects: function {} requires undeclared effect alloc",
                self.symbol_display(root, &entry.symbol)?
            );
        }
        if self.expr_requires_unsafe(&body)? && !declared.contains(&Effect::Unsafe) {
            bail!(
                "bad_effects: function {} requires undeclared effect unsafe",
                self.symbol_display(root, &entry.symbol)?
            );
        }
        if self.expr_requires_io(&body)? && !declared.contains(&Effect::Io) {
            bail!(
                "bad_effects: function {} requires undeclared effect io",
                self.symbol_display(root, &entry.symbol)?
            );
        }
        let dependencies = self.dependencies_for_definition(root, &entry.definition)?;
        for dependency in dependencies {
            let Some(callee) = self.root_symbol(root, &dependency) else {
                continue;
            };
            for effect in self.signature_effects(&callee.signature)? {
                if !declared.contains(&effect) {
                    bail!(
                        "bad_effects: function {} calls {} with undeclared effect {}",
                        self.symbol_display(root, &entry.symbol)?,
                        self.symbol_display(root, &dependency)?,
                        effect.as_str()
                    );
                }
            }
        }
        Ok(())
    }

    fn verify_function_borrows(
        &self,
        root: &ProgramRootPayload,
        entry: &crate::model::RootSymbolPayload,
        param_types: &[String],
    ) -> Result<()> {
        let body = self.function_body_hash(&entry.definition)?;
        let mut state = MoveBorrowState {
            locals: Vec::new(),
            active: Vec::new(),
            moved: Vec::new(),
            next_local: 0,
            iteration_scope: None,
        };
        self.verify_expr_borrows(root, &body, param_types, &mut state, ExprUse::Value)
            .with_context(|| {
                format!(
                    "bad_borrow: function {} violates borrow rules",
                    self.symbol_display(root, &entry.symbol)
                        .unwrap_or(entry.symbol.clone())
                )
            })
    }

    /// Whether the typed expression `expr_hash` always exits the enclosing
    /// function early (R7) — the typed-DAG counterpart of
    /// [`crate::expr::raw_expr_diverges`]. A divergent expression yields no value
    /// to its own context, so the borrow/move merge and lowering route a divergent
    /// branch to the early-exit edge instead of the join. Used wherever a branch's
    /// state must NOT flow into the continuation it never reaches.
    pub(crate) fn typed_expr_diverges(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        let kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        Ok(match kind {
            "return" => true,
            "if" => {
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                self.typed_expr_diverges(then_hash)? && self.typed_expr_diverges(else_hash)?
            }
            "case" => {
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?;
                if arms.is_empty() {
                    return Ok(false);
                }
                for arm in arms {
                    let body = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    if !self.typed_expr_diverges(body)? {
                        return Ok(false);
                    }
                }
                true
            }
            "let" => {
                let body = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing body"))?;
                self.typed_expr_diverges(body)?
            }
            _ => false,
        })
    }

    fn verify_expr_borrows(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        state: &mut MoveBorrowState,
        expr_use: ExprUse,
    ) -> Result<()> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" => Ok(()),
            "param_ref" | "local_ref" => match expr_use {
                ExprUse::Place | ExprUse::ProjectionBase => {
                    let place = self.loan_place_for_expr(expr_hash, param_types, &state.locals)?;
                    self.check_place_not_moved_for_use(&place, state, expr_use)
                }
                ExprUse::Value => self.verify_place_value_use(root, expr_hash, param_types, state),
            },
            "call" => {
                let (args, _, _) = self.call_region_context(root, &payload)?;
                let mut added_call_loans = Vec::new();
                let mut moved_call_owners = Vec::new();
                for arg in args {
                    let pre_arg_state = state.clone();
                    let transfer_owners = self.move_source_places_for_expr(
                        root,
                        &arg,
                        param_types,
                        &pre_arg_state.locals,
                    )?;
                    self.verify_expr_borrows(root, &arg, param_types, state, ExprUse::Value)?;
                    let arg_loans =
                        self.collect_value_loans(root, &arg, param_types, &pre_arg_state)?;
                    self.check_loans_point_to_live_storage(&arg_loans, state)?;
                    state.active.retain(|loan| {
                        !transfer_owners
                            .iter()
                            .any(|owner| loan_owner_overlaps(loan, owner))
                    });
                    moved_call_owners.extend(transfer_owners);
                    added_call_loans
                        .extend(self.add_checked_value_loans(&mut state.active, &arg_loans)?);
                }
                state.active.retain(|loan| {
                    !added_call_loans.contains(loan)
                        && !moved_call_owners
                            .iter()
                            .any(|owner| loan_owner_overlaps(loan, owner))
                });
                Ok(())
            }
            "binary" => {
                for key in ["left", "right"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("binary missing {key}"))?;
                    self.verify_expr_borrows(root, child, param_types, state, ExprUse::Value)?;
                }
                Ok(())
            }
            "unary" => {
                let child = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?;
                self.verify_expr_borrows(root, child, param_types, state, ExprUse::Value)
            }
            "int_cast" => {
                // A cast reads its (Copy scalar) argument as a value; it never moves
                // or borrows, so it just borrow-checks the operand.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing value"))?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)
            }
            "borrow_shared" | "borrow_mut" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow expression missing target"))?;
                let kind =
                    if payload.get("expr_kind").and_then(JsonValue::as_str) == Some("borrow_mut") {
                        LoanKind::Mutable
                    } else {
                        LoanKind::Shared
                    };
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&place, state)?;
                self.check_loan_conflicts(&kind, &place, true, &state.active)?;
                Ok(())
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
                let kind = if mutable {
                    LoanKind::Mutable
                } else {
                    LoanKind::Shared
                };
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&place, state)?;
                self.check_loan_conflicts(&kind, &place, true, &state.active)?;
                Ok(())
            }
            "slice_len" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_len missing target"))?;
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Value)
            }
            "subslice" => {
                for key in ["target", "start", "len"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("subslice missing {key}"))?;
                    self.verify_expr_borrows(root, child, param_types, state, ExprUse::Value)?;
                }
                let value_loans = self.collect_value_loans(root, expr_hash, param_types, state)?;
                self.check_loans_point_to_live_storage(&value_loans, state)
            }
            "box_new" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("box_new missing value"))?;
                let pre_value_state = state.clone();
                let transfer_owners = self.move_source_places_for_expr(
                    root,
                    value,
                    param_types,
                    &pre_value_state.locals,
                )?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)?;
                state.active.retain(|loan| {
                    !transfer_owners
                        .iter()
                        .any(|owner| loan_owner_overlaps(loan, owner))
                });
                Ok(())
            }
            "unbox" => {
                // `unbox` consumes (moves) its `box` argument, exactly like `box_new`
                // consumes the value it boxes: collect the moved place, borrow-check
                // the argument as a value, and retire loans owned by the moved box.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unbox missing value"))?;
                let pre_value_state = state.clone();
                let transfer_owners = self.move_source_places_for_expr(
                    root,
                    value,
                    param_types,
                    &pre_value_state.locals,
                )?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)?;
                state.active.retain(|loan| {
                    !transfer_owners
                        .iter()
                        .any(|owner| loan_owner_overlaps(loan, owner))
                });
                Ok(())
            }
            "vec_new" => {
                let capacity = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_new missing capacity"))?;
                self.verify_expr_borrows(root, capacity, param_types, state, ExprUse::Value)
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
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let target_place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&target_place, state)?;
                self.check_loan_conflicts(&LoanKind::Mutable, &target_place, true, &state.active)?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)
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
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&place, state)?;
                self.check_shared_read_conflicts(&place, &state.active)?;
                self.verify_expr_borrows(root, index, param_types, state, ExprUse::Value)
            }
            "vec_len" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_len missing target"))?;
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&place, state)?;
                self.check_shared_read_conflicts(&place, &state.active)
            }
            "string_new" => {
                let source = payload
                    .get("source")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_new missing source"))?;
                self.verify_expr_borrows(root, source, param_types, state, ExprUse::Value)
            }
            "string_len" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_len missing target"))?;
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&place, state)?;
                self.check_shared_read_conflicts(&place, &state.active)
            }
            "string_with_capacity" => {
                let capacity = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_with_capacity missing capacity"))?;
                self.verify_expr_borrows(root, capacity, param_types, state, ExprUse::Value)
            }
            "arg_count" => Ok(()),
            "arg_len" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("arg_len missing index"))?;
                self.verify_expr_borrows(root, index, param_types, state, ExprUse::Value)
            }
            "arg_byte" => {
                for key in ["index", "byte"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("arg_byte missing {key}"))?;
                    self.verify_expr_borrows(root, child, param_types, state, ExprUse::Value)?;
                }
                Ok(())
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
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let target_place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&target_place, state)?;
                self.check_loan_conflicts(&LoanKind::Mutable, &target_place, true, &state.active)?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)
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
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&place, state)?;
                self.check_shared_read_conflicts(&place, &state.active)?;
                self.verify_expr_borrows(root, index, param_types, state, ExprUse::Value)
            }
            "string_set" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_set missing target"))?;
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_set missing index"))?;
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_set missing value"))?;
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let target_place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&target_place, state)?;
                self.check_loan_conflicts(&LoanKind::Mutable, &target_place, true, &state.active)?;
                self.verify_expr_borrows(root, index, param_types, state, ExprUse::Value)?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)
            }
            "raw_ptr_cast" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)
            }
            "raw_load" => {
                let pointer = payload
                    .get("pointer")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_load missing pointer"))?;
                self.verify_expr_borrows(root, pointer, param_types, state, ExprUse::Value)
            }
            "raw_store" => {
                for key in ["pointer", "value"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("raw_store missing {key}"))?;
                    self.verify_expr_borrows(root, child, param_types, state, ExprUse::Value)?;
                }
                Ok(())
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
                let pre_value_state = state.clone();
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let target_place = self.loan_place_for_expr(target, param_types, &state.locals)?;
                self.check_place_not_moved(&target_place, state)?;
                self.check_loan_conflicts(&LoanKind::Mutable, &target_place, true, &state.active)?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)?;
                self.check_place_not_moved(&target_place, state)?;
                let target_type = self.expr_declared_type(target)?;
                let target_class = self.value_class_in_root(root, &target_type)?;
                if target_class.contains_reference {
                    let value_loans = self.collect_value_loans_for_store(
                        root,
                        value,
                        param_types,
                        &pre_value_state,
                        &target_place,
                    )?;
                    self.check_loans_point_to_live_storage(&value_loans, state)?;
                    // Reassigning a reference-carrying place ends the loans that
                    // the overwritten place (and its sub-places) carried, then
                    // attributes the stored value's loans to it. Only retire
                    // loans owned by the assigned place or below it. A loan owned
                    // by a STRICT ANCESTOR of the assigned place is tracked at a
                    // coarser (whole-aggregate) granularity than the assignment:
                    // ending it would also discard the sibling fields' loans it
                    // represents — the aliasing-&mut hole — so reject fail-closed
                    // rather than silently drop them. Values built per-field via a
                    // record literal carry per-field owners and take the precise
                    // path.
                    let mut retained = Vec::with_capacity(state.active.len());
                    for loan in state.active.drain(..) {
                        match loan.owner.as_ref() {
                            // Owned by the assigned place or a concrete sub-place
                            // of it: this storage is definitely overwritten, so
                            // its loan ends. Use EXACT matching — a dynamic `[*]`
                            // index proves nothing about which element is hit, so
                            // it must not be treated as overwriting a concrete
                            // sibling `[N]` here (doing so would end a loan whose
                            // referent may still be live — an aliasing-&mut hole).
                            Some(owner) if place_is_prefix_of_exact(&target_place, owner) => {
                                // Owned by the assigned place or a sub-place: retire.
                            }
                            Some(owner)
                                if place_is_prefix_of_exact(owner, &target_place)
                                    && owner != &target_place =>
                            {
                                bail!(
                                    "unsupported_assign: cannot reassign reference place {:?}; its loans are tracked at the coarser granularity {:?}. Build the value with a record literal so per-field loans are tracked.",
                                    target_place,
                                    owner
                                );
                            }
                            // Neither place is a provable prefix of the other yet
                            // they still overlap — only possible through a dynamic
                            // `[*]` index (e.g. `arr[i] = ...` aliasing a loan
                            // owned by `arr[1]`). We cannot prove the loan is
                            // overwritten (so retiring it is unsound) nor that it
                            // survives, so reject fail-closed rather than guess.
                            Some(owner) if places_overlap(&target_place, owner) => {
                                bail!(
                                    "unsupported_assign: cannot reassign reference place {:?} through a dynamic index; its element loans cannot be tracked precisely. Use a constant index.",
                                    target_place
                                );
                            }
                            _ => retained.push(loan),
                        }
                    }
                    state.active = retained;
                    self.add_checked_value_loans(&mut state.active, &value_loans)?;
                }
                Ok(())
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
                let pre_value_state = state.clone();
                self.verify_expr_borrows(root, value_hash, param_types, state, ExprUse::Value)?;
                let transfer_owners = self.move_source_places_for_expr(
                    root,
                    value_hash,
                    param_types,
                    &pre_value_state.locals,
                )?;
                let local_id = state.next_local;
                state.next_local += 1;
                let local_owner = LoanPlace {
                    root: LoanRoot::Local(local_id),
                    fields: Vec::new(),
                };
                let value_loans = self.collect_value_loans_for_store(
                    root,
                    value_hash,
                    param_types,
                    &pre_value_state,
                    &local_owner,
                )?;
                self.check_loans_point_to_live_storage(&value_loans, state)?;
                state.active.retain(|loan| {
                    !transfer_owners
                        .iter()
                        .any(|owner| loan_owner_overlaps(loan, owner))
                });
                state.locals.push(local_id);
                self.add_checked_value_loans(&mut state.active, &value_loans)?;
                let body_result =
                    self.verify_expr_borrows(root, body_hash, param_types, state, ExprUse::Value);
                state
                    .active
                    .retain(|loan| !loan_owner_overlaps(loan, &local_owner));
                let scope_result = if body_result.is_ok() {
                    self.check_no_loans_outlive_local(local_id, state)
                } else {
                    Ok(())
                };
                state
                    .moved
                    .retain(|place| !matches!(place.root, LoanRoot::Local(id) if id == local_id));
                state.locals.pop();
                body_result.and(scope_result)
            }
            "if" => {
                let cond = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing cond"))?;
                self.verify_expr_borrows(root, cond, param_types, state, ExprUse::Value)?;
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                let boundary = state.next_local;
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                self.verify_expr_borrows(
                    root,
                    then_hash,
                    param_types,
                    &mut then_state,
                    ExprUse::Value,
                )?;
                self.verify_expr_borrows(
                    root,
                    else_hash,
                    param_types,
                    &mut else_state,
                    ExprUse::Value,
                )?;
                // Conditional drop glue (SPEC_V3 §7): an owned value moved in one
                // branch but not the other is now supported — lowering emits
                // compensating drops so each path drops it exactly once. The
                // merged move set is the union of both branches, so a use of a
                // place moved on either path is still rejected as a potential
                // use-after-move. `boundary` is retained for symmetry with the
                // branch-local reasoning in lowering.
                let _ = boundary;
                // Early exit (R7): a branch that always `return`s never reaches the
                // code after the `if`, so its moves must NOT flow into the
                // continuation — only the non-divergent branch's state does (else a
                // place merely moved on the early-exit path would falsely read as
                // moved afterward). Both branches are still checked above.
                let then_div = self.typed_expr_diverges(then_hash)?;
                let else_div = self.typed_expr_diverges(else_hash)?;
                match (then_div, else_div) {
                    (false, false) => merge_branch_state(state, then_state, else_state),
                    (true, false) => *state = else_state,
                    (false, true) => *state = then_state,
                    // Both diverge: the code after the `if` is unreachable; pick one
                    // consistent state for the (dead) continuation's checks.
                    (true, true) => *state = then_state,
                }
                Ok(())
            }
            "fold" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing target"))?;
                let init = payload
                    .get("init")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing init"))?;
                let body = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing body"))?;
                let element_type = payload
                    .get("element_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing element_type"))?;
                let acc_type = payload
                    .get("acc_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing acc_type"))?;
                if self
                    .value_class_in_root(root, element_type)?
                    .contains_reference
                    || self.value_class_in_root(root, acc_type)?.contains_reference
                {
                    bail!(
                        "fold element and accumulator types must not carry references in phase 13"
                    );
                }
                let target_is_place = self.typed_expr_is_place(target)?;
                let target_use = if target_is_place {
                    ExprUse::Place
                } else {
                    ExprUse::Value
                };
                self.verify_expr_borrows(root, target, param_types, state, target_use)?;
                if payload.get("target_kind").and_then(JsonValue::as_str) == Some("fixed_array")
                    && target_is_place
                {
                    let target_place =
                        self.loan_place_for_expr(target, param_types, &state.locals)?;
                    self.check_shared_read_conflicts(&target_place, &state.active)?;
                }
                self.verify_expr_borrows(root, init, param_types, state, ExprUse::Value)?;
                let item_local = state.next_local;
                state.next_local += 1;
                state.locals.push(item_local);
                let acc_local = state.next_local;
                state.next_local += 1;
                state.locals.push(acc_local);
                // The body runs once per element (#14a → loop-carried drop
                // glue): per-iteration locals may move (their scoped drop glue
                // re-executes each element); the item, the accumulator, and
                // anything outer may not (the move would repeat). Mirrors the
                // lowering gate so eval stays a faithful oracle of the native
                // envelope.
                let outer_scope = state.iteration_scope;
                state.iteration_scope = Some(IterationMoveScope {
                    floor: acc_local + 1,
                    movable_whole: None,
                    construct: "fold body",
                });
                let body_result =
                    self.verify_expr_borrows(root, body, param_types, state, ExprUse::Value);
                state.iteration_scope = outer_scope;
                let item_owner = LoanPlace {
                    root: LoanRoot::Local(item_local),
                    fields: Vec::new(),
                };
                let acc_owner = LoanPlace {
                    root: LoanRoot::Local(acc_local),
                    fields: Vec::new(),
                };
                state.active.retain(|loan| {
                    !loan_owner_overlaps(loan, &item_owner)
                        && !loan_owner_overlaps(loan, &acc_owner)
                });
                state.moved.retain(|place| {
                    !matches!(place.root, LoanRoot::Local(id) if id == item_local || id == acc_local)
                });
                state.locals.pop();
                state.locals.pop();
                body_result
            }
            "loop" => {
                let init = payload
                    .get("init")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing init"))?;
                let cond = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing cond"))?;
                let body = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing body"))?;
                let acc_type = payload
                    .get("acc_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing acc_type"))?;
                if self.value_class_in_root(root, acc_type)?.contains_reference {
                    bail!("loop accumulator type must not carry references");
                }
                self.verify_expr_borrows(root, init, param_types, state, ExprUse::Value)?;
                // `acc` is a loop-local; `cond` and `body` see it. One representative
                // iteration is checked (cond then body, sharing state); the
                // accumulator's loans/moves are loop-local and retired afterward.
                let acc_local = state.next_local;
                state.next_local += 1;
                state.locals.push(acc_local);
                // `cond`/`body` run 0..N times (#14a → loop-carried drop glue):
                // per-iteration locals may move (scoped drop glue re-executes
                // each iteration); outer places may not (the move would
                // repeat). The BODY may additionally consume the accumulator
                // wholly — the back-edge refills it, and lowering drops the old
                // value when the body did not consume it. The CONDITION may
                // not: it runs once more than the body, and its final
                // evaluation would leave the loop's result consumed.
                let outer_scope = state.iteration_scope;
                state.iteration_scope = Some(IterationMoveScope {
                    floor: acc_local + 1,
                    movable_whole: None,
                    construct: "loop condition",
                });
                let cond_result =
                    self.verify_expr_borrows(root, cond, param_types, state, ExprUse::Value);
                state.iteration_scope = Some(IterationMoveScope {
                    floor: acc_local + 1,
                    movable_whole: Some(acc_local),
                    construct: "loop body",
                });
                let body_result =
                    self.verify_expr_borrows(root, body, param_types, state, ExprUse::Value);
                state.iteration_scope = outer_scope;
                let acc_owner = LoanPlace {
                    root: LoanRoot::Local(acc_local),
                    fields: Vec::new(),
                };
                state
                    .active
                    .retain(|loan| !loan_owner_overlaps(loan, &acc_owner));
                state
                    .moved
                    .retain(|place| !matches!(place.root, LoanRoot::Local(id) if id == acc_local));
                state.locals.pop();
                cond_result.and(body_result)
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
                    self.verify_expr_borrows(root, child, param_types, state, ExprUse::Value)?;
                }
                let value_loans = self.collect_value_loans(root, expr_hash, param_types, state)?;
                self.check_loans_point_to_live_storage(&value_loans, state)?;
                // A field value may *move* an existing move-only binding into the
                // record; that binding's loan is being transferred into the new
                // value, not aliased. Retire the moved sources from the working
                // copy before checking carried-loan conflicts so the transfer
                // does not conflict with itself, while still catching genuine
                // duplicates (e.g. two `&mut x` fields, which are not moves).
                let transfer_owners =
                    self.move_source_places_for_expr(root, expr_hash, param_types, &state.locals)?;
                let mut active = state.active.clone();
                active.retain(|loan| {
                    !transfer_owners
                        .iter()
                        .any(|owner| loan_owner_overlaps(loan, owner))
                });
                self.add_checked_value_loans(&mut active, &value_loans)?;
                Ok(())
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
                    self.verify_expr_borrows(root, child, param_types, state, ExprUse::Value)?;
                }
                let value_loans = self.collect_value_loans(root, expr_hash, param_types, state)?;
                self.check_loans_point_to_live_storage(&value_loans, state)?;
                let transfer_owners =
                    self.move_source_places_for_expr(root, expr_hash, param_types, &state.locals)?;
                let mut active = state.active.clone();
                active.retain(|loan| {
                    !transfer_owners
                        .iter()
                        .any(|owner| loan_owner_overlaps(loan, owner))
                });
                self.add_checked_value_loans(&mut active, &value_loans)?;
                Ok(())
            }
            "array_fill" => {
                // `[value; count]`: `value` is evaluated once (a non-reference Copy
                // value, by the type rule), so its borrow behaviour IS the whole
                // expression's — no per-slot loan or move bookkeeping is needed.
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                self.verify_expr_borrows(root, value_hash, param_types, state, ExprUse::Value)
            }
            "array_set" => {
                // `array_set(arr, i, v)`: arr/i/v are each read as values (the
                // element type rule forbids references, so the result Copy array
                // carries no loan) — so the borrow behaviour is just the children's,
                // with no per-element loan or move bookkeeping (mirrors `binary`).
                for key in ["array", "index", "value"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array_set missing {key}"))?;
                    self.verify_expr_borrows(root, child, param_types, state, ExprUse::Value)?;
                }
                Ok(())
            }
            "field_access" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::ProjectionBase)?;
                let place = self.loan_place_for_expr(expr_hash, param_types, &state.locals)?;
                self.check_place_not_moved_for_use(&place, state, expr_use)?;
                match expr_use {
                    ExprUse::Place | ExprUse::ProjectionBase => Ok(()),
                    ExprUse::Value => {
                        self.check_shared_read_conflicts(&place, &state.active)?;
                        self.verify_place_value_use(root, expr_hash, param_types, state)
                    }
                }
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
                self.verify_expr_borrows(root, index, param_types, state, ExprUse::Value)?;
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::ProjectionBase)?;
                let place = self.loan_place_for_expr(expr_hash, param_types, &state.locals)?;
                self.check_place_not_moved_for_use(&place, state, expr_use)?;
                match expr_use {
                    ExprUse::Place | ExprUse::ProjectionBase => Ok(()),
                    ExprUse::Value => {
                        self.check_shared_read_conflicts(&place, &state.active)?;
                        self.verify_place_value_use(root, expr_hash, param_types, state)
                    }
                }
            }
            "enum_construct" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)
            }
            "case" => {
                let expr = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                self.verify_expr_borrows(root, expr, param_types, state, ExprUse::Value)?;
                let base_state = state.clone();
                // Conditional drop glue (SPEC_V3 §7): an owned value moved in only
                // some `case` arms is supported — lowering emits compensating
                // drops so each arm drops it exactly once. The merged move set is
                // the union across arms, so a later use of a place moved on any
                // arm is still rejected as a potential use-after-move.
                let mut merged: Option<MoveBorrowState> = None;
                for arm in payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?
                {
                    let body = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    // A nested pattern's leaf binding (R14) is a local too; it owns a
                    // sub-component of the fully-consumed scrutinee, so the loan
                    // modeling below (sourced from the scrutinee `expr`) covers it.
                    let pushed = crate::expr::typed_arm_binding_name(arm).is_some();
                    let mut arm_state = base_state.clone();
                    if pushed {
                        let local_id = arm_state.next_local;
                        arm_state.next_local += 1;
                        arm_state.locals.push(local_id);
                        let local_owner = LoanPlace {
                            root: LoanRoot::Local(local_id),
                            fields: Vec::new(),
                        };
                        let transfer_owners = self.move_source_places_for_expr(
                            root,
                            expr,
                            param_types,
                            &base_state.locals,
                        )?;
                        let value_loans = self.collect_value_loans_for_store(
                            root,
                            expr,
                            param_types,
                            &base_state,
                            &local_owner,
                        )?;
                        self.check_loans_point_to_live_storage(&value_loans, &arm_state)?;
                        arm_state.active.retain(|loan| {
                            !transfer_owners
                                .iter()
                                .any(|owner| loan_owner_overlaps(loan, owner))
                        });
                        self.add_checked_value_loans(&mut arm_state.active, &value_loans)?;
                    }
                    if pushed {
                        let local_id = *arm_state
                            .locals
                            .last()
                            .ok_or_else(|| anyhow!("case binding local missing"))?;
                        self.verify_expr_borrows(
                            root,
                            body,
                            param_types,
                            &mut arm_state,
                            ExprUse::Value,
                        )?;
                        let local_owner = LoanPlace {
                            root: LoanRoot::Local(local_id),
                            fields: Vec::new(),
                        };
                        arm_state
                            .active
                            .retain(|loan| !loan_owner_overlaps(loan, &local_owner));
                        arm_state.moved.retain(
                            |place| !matches!(place.root, LoanRoot::Local(id) if id == local_id),
                        );
                        arm_state.locals.pop();
                    } else {
                        // The guard (R14, scalar arms only) runs before the body in the
                        // same scope; borrow-check it first. A pure, bool-returning
                        // guard yields only transient loans (none escape into `body`).
                        if let Some(guard) = arm.get("guard").and_then(JsonValue::as_str) {
                            self.verify_expr_borrows(
                                root,
                                guard,
                                param_types,
                                &mut arm_state,
                                ExprUse::Value,
                            )?;
                        }
                        self.verify_expr_borrows(
                            root,
                            body,
                            param_types,
                            &mut arm_state,
                            ExprUse::Value,
                        )?;
                    }
                    // Early exit (R7): a divergent arm never reaches the code after
                    // the `case`, so its moves do not flow into the continuation —
                    // only non-divergent arms merge (the arm is still checked above).
                    if !self.typed_expr_diverges(body)? {
                        merged = Some(match merged {
                            Some(previous) => merged_branch_states(previous, arm_state),
                            None => arm_state,
                        });
                    }
                }
                if let Some(merged) = merged {
                    *state = merged;
                }
                Ok(())
            }
            "return" => {
                // Early exit (R7): the returned value is consumed (moved) out of the
                // function. Borrow/move-check it as an owned value; the divergence of
                // this branch is handled by the enclosing `if`/`case` merge, which
                // does not flow this branch's state into the continuation.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("return missing value"))?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)?;
                Ok(())
            }
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn collect_value_loans(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        state: &MoveBorrowState,
    ) -> Result<Vec<ActiveLoan>> {
        let mut out = Vec::new();
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "borrow_shared" | "borrow_mut" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow expression missing target"))?;
                let region = payload
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow expression missing region"))?
                    .to_string();
                out.push(ActiveLoan {
                    kind: if payload.get("expr_kind").and_then(JsonValue::as_str)
                        == Some("borrow_mut")
                    {
                        LoanKind::Mutable
                    } else {
                        LoanKind::Shared
                    },
                    region,
                    place: self.loan_place_for_expr(target, param_types, &state.locals)?,
                    owner: None,
                    exclusive: true,
                });
            }
            "slice_from_array" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_from_array missing target"))?;
                let region = payload
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_from_array missing region"))?
                    .to_string();
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("slice_from_array missing mutable"))?;
                out.push(ActiveLoan {
                    kind: if mutable {
                        LoanKind::Mutable
                    } else {
                        LoanKind::Shared
                    },
                    region,
                    place: self.loan_place_for_expr(target, param_types, &state.locals)?,
                    owner: None,
                    exclusive: true,
                });
            }
            "subslice" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing target"))?;
                out.extend(self.collect_value_loans(root, target, param_types, state)?);
            }
            "box_new" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("box_new missing value"))?;
                out.extend(self.collect_value_loans(root, value, param_types, state)?);
            }
            "unbox" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unbox missing value"))?;
                out.extend(self.collect_value_loans(root, value, param_types, state)?);
            }
            "int_cast" => {
                // The cast result is a scalar (carries no loan); propagate any loan
                // produced while evaluating the operand.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing value"))?;
                out.extend(self.collect_value_loans(root, value, param_types, state)?);
            }
            "raw_ptr_cast" => {
                // Converting a borrow of a place into a raw pointer erases its
                // region (SPEC §15), but the raw pointer still points into that
                // storage. Keep carrying the underlying loan so liveness/escape
                // checks (`check_loans_point_to_live_storage`,
                // `check_no_loans_outlive_local`) fire when the raw pointer would
                // outlive the place it borrows — otherwise a raw cast launders an
                // escaping borrow of a local. Mark the carried loans non-exclusive:
                // raw pointers may legally alias under `unsafe`, so they must not
                // trigger the aliasing/exclusivity checks.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?;
                out.extend(
                    self.collect_value_loans(root, value, param_types, state)?
                        .into_iter()
                        .map(|mut loan| {
                            loan.exclusive = false;
                            loan
                        }),
                );
            }
            "call" => {
                let (args, return_type, _) = self.call_region_context(root, &payload)?;
                let return_regions = self.reference_regions_in_type(root, &return_type)?;
                // A call whose return type contains a raw pointer may return a
                // region-erased pointer derived from any raw-pointer argument
                // (we cannot prove otherwise without raw-pointer provenance), so
                // we conservatively carry such arguments' loans out of the call.
                let return_has_raw =
                    self.type_contains_raw_pointer(Some(root), &return_type, &mut BTreeSet::new())?;
                if return_regions.is_empty() && !return_has_raw {
                    return Ok(out);
                }
                for arg in args {
                    let arg_carries_raw = return_has_raw
                        && self.type_contains_raw_pointer(
                            Some(root),
                            &self.expr_declared_type(&arg)?,
                            &mut BTreeSet::new(),
                        )?;
                    out.extend(
                        self.collect_value_loans(root, &arg, param_types, state)?
                            .into_iter()
                            .filter(|loan| {
                                arg_carries_raw || return_regions.contains(&loan.region)
                            }),
                    );
                }
            }
            "record_literal" => {
                for field in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                {
                    let value = field
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing value"))?;
                    out.extend(self.collect_value_loans(root, value, param_types, state)?);
                }
            }
            "array_literal" => {
                for element in payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                {
                    let value = element
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing value"))?;
                    out.extend(self.collect_value_loans(root, value, param_types, state)?);
                }
            }
            "array_fill" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                out.extend(self.collect_value_loans(root, value, param_types, state)?);
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
                let mut nested_state = state.clone();
                let transfer_owners = self.move_source_places_for_expr(
                    root,
                    value_hash,
                    param_types,
                    &nested_state.locals,
                )?;
                let local_id = nested_state.next_local;
                nested_state.next_local += 1;
                let local_owner = LoanPlace {
                    root: LoanRoot::Local(local_id),
                    fields: Vec::new(),
                };
                let value_loans = self.collect_value_loans_for_store(
                    root,
                    value_hash,
                    param_types,
                    &nested_state,
                    &local_owner,
                )?;
                nested_state.active.retain(|loan| {
                    !transfer_owners
                        .iter()
                        .any(|owner| loan_owner_overlaps(loan, owner))
                });
                nested_state.locals.push(local_id);
                nested_state.active.extend(value_loans);
                let body_loans =
                    self.collect_value_loans(root, body_hash, param_types, &nested_state)?;
                self.check_loans_point_to_live_storage(&body_loans, state)?;
                out.extend(body_loans);
            }
            // `return <value>` (R7) yields no value to its context — it exits the
            // function — so it contributes no loans to a binding/merge here. The
            // loans the operand carries escape as a return value and are governed by
            // `expr_escapes_local_borrow`, not by this binding-loan flow.
            "return" => {}
            "if" => {
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                out.extend(alternative_value_loans(
                    self.collect_value_loans(root, then_hash, param_types, state)?,
                    self.collect_value_loans(root, else_hash, param_types, state)?,
                ));
            }
            "enum_construct" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                out.extend(self.collect_value_loans(root, value, param_types, state)?);
            }
            "case" => {
                let expr = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                let base_state = state.clone();
                for arm in payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?
                {
                    let body = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    let mut arm_state = base_state.clone();
                    // A nested pattern's leaf binding (R14) is a scoped local too.
                    if crate::expr::typed_arm_binding_name(arm).is_some() {
                        let local_id = arm_state.next_local;
                        arm_state.next_local += 1;
                        let local_owner = LoanPlace {
                            root: LoanRoot::Local(local_id),
                            fields: Vec::new(),
                        };
                        let transfer_owners = self.move_source_places_for_expr(
                            root,
                            expr,
                            param_types,
                            &arm_state.locals,
                        )?;
                        let value_loans = self.collect_value_loans_for_store(
                            root,
                            expr,
                            param_types,
                            &arm_state,
                            &local_owner,
                        )?;
                        arm_state.active.retain(|loan| {
                            !transfer_owners
                                .iter()
                                .any(|owner| loan_owner_overlaps(loan, owner))
                        });
                        arm_state.locals.push(local_id);
                        arm_state.active.extend(value_loans);
                    }
                    let body_loans =
                        self.collect_value_loans(root, body, param_types, &arm_state)?;
                    self.check_loans_point_to_live_storage(&body_loans, state)?;
                    out.extend(body_loans);
                }
            }
            "param_ref" | "local_ref" | "field_access" | "array_index" => {
                let type_hash = self.expr_declared_type(expr_hash)?;
                let class = self.value_class_in_root(root, &type_hash)?;
                if class.contains_reference {
                    let owner = self.loan_place_for_expr(expr_hash, param_types, &state.locals)?;
                    out.extend(
                        state
                            .active
                            .iter()
                            .filter(|loan| loan_owner_overlaps(loan, &owner))
                            .cloned(),
                    );
                }
            }
            _ => {}
        }
        Ok(out)
    }

    fn collect_value_loans_for_store(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        state: &MoveBorrowState,
        target_owner: &LoanPlace,
    ) -> Result<Vec<ActiveLoan>> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "record_literal" => {
                let mut out = Vec::new();
                for field in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                {
                    let name = field
                        .get("name")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing name"))?;
                    let value = field
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing value"))?;
                    let field_owner = target_owner.with_field(name);
                    out.extend(self.collect_value_loans_for_store(
                        root,
                        value,
                        param_types,
                        state,
                        &field_owner,
                    )?);
                }
                Ok(out)
            }
            "array_literal" => {
                let mut out = Vec::new();
                for (idx, element) in payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                    .iter()
                    .enumerate()
                {
                    let value = element
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing value"))?;
                    let element_owner = target_owner.with_segment(array_index_segment(idx as u64));
                    out.extend(self.collect_value_loans_for_store(
                        root,
                        value,
                        param_types,
                        state,
                        &element_owner,
                    )?);
                }
                Ok(out)
            }
            "array_fill" => {
                // A non-reference Copy value carries no loan to replicate into the
                // slots; recursing reports the value's own (empty) store loans.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                self.collect_value_loans_for_store(root, value, param_types, state, target_owner)
            }
            "if" => {
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                Ok(alternative_value_loans(
                    self.collect_value_loans_for_store(
                        root,
                        then_hash,
                        param_types,
                        state,
                        target_owner,
                    )?,
                    self.collect_value_loans_for_store(
                        root,
                        else_hash,
                        param_types,
                        state,
                        target_owner,
                    )?,
                ))
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
                let mut nested_state = state.clone();
                let transfer_owners = self.move_source_places_for_expr(
                    root,
                    value_hash,
                    param_types,
                    &nested_state.locals,
                )?;
                let local_id = nested_state.next_local;
                nested_state.next_local += 1;
                let local_owner = LoanPlace {
                    root: LoanRoot::Local(local_id),
                    fields: Vec::new(),
                };
                let value_loans = self.collect_value_loans_for_store(
                    root,
                    value_hash,
                    param_types,
                    &nested_state,
                    &local_owner,
                )?;
                nested_state.active.retain(|loan| {
                    !transfer_owners
                        .iter()
                        .any(|owner| loan_owner_overlaps(loan, owner))
                });
                nested_state.locals.push(local_id);
                nested_state.active.extend(value_loans);
                self.collect_value_loans_for_store(
                    root,
                    body_hash,
                    param_types,
                    &nested_state,
                    target_owner,
                )
            }
            "case" => {
                let expr = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                let mut out = Vec::new();
                for arm in payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?
                {
                    let body = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    let mut arm_state = state.clone();
                    // A nested pattern's leaf binding (R14) is a scoped local too.
                    if crate::expr::typed_arm_binding_name(arm).is_some() {
                        let local_id = arm_state.next_local;
                        arm_state.next_local += 1;
                        let local_owner = LoanPlace {
                            root: LoanRoot::Local(local_id),
                            fields: Vec::new(),
                        };
                        let transfer_owners = self.move_source_places_for_expr(
                            root,
                            expr,
                            param_types,
                            &arm_state.locals,
                        )?;
                        let value_loans = self.collect_value_loans_for_store(
                            root,
                            expr,
                            param_types,
                            &arm_state,
                            &local_owner,
                        )?;
                        arm_state.active.retain(|loan| {
                            !transfer_owners
                                .iter()
                                .any(|owner| loan_owner_overlaps(loan, owner))
                        });
                        arm_state.locals.push(local_id);
                        arm_state.active.extend(value_loans);
                    }
                    out.extend(self.collect_value_loans_for_store(
                        root,
                        body,
                        param_types,
                        &arm_state,
                        target_owner,
                    )?);
                }
                Ok(out)
            }
            // `return <value>` (R7) exits the function rather than storing into the
            // binding, so it transfers no loans to the store target (it is not a
            // place; the catch-all's place attribution does not apply).
            "return" => Ok(Vec::new()),
            _ => {
                let mut loans = self.collect_value_loans(root, expr_hash, param_types, state)?;
                let source_owner =
                    self.source_place_for_value_expr(expr_hash, param_types, &state.locals)?;
                // When the stored value is not itself a place (e.g. a call
                // result), its carried loans have no per-field source structure
                // to preserve. If the value's type has exactly one
                // reference-bearing field position, attribute every carried loan
                // to that field so a later single-field reassignment can retire
                // it precisely. Otherwise fall back to whole-value attribution,
                // which makes a partial reassignment fail closed rather than
                // silently drop a sibling field's loan (aliasing-&mut hole).
                let attributed_owner = if source_owner.is_none() {
                    let value_type = self.expr_declared_type(expr_hash)?;
                    match self.single_reference_field_path(root, &value_type)? {
                        Some(path) if !path.is_empty() => {
                            let mut owner = target_owner.clone();
                            owner.fields.extend(path);
                            Some(owner)
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                for loan in &mut loans {
                    loan.owner = Some(match attributed_owner.as_ref() {
                        Some(owner) => owner.clone(),
                        None => rebased_loan_owner(
                            loan.owner.as_ref(),
                            source_owner.as_ref(),
                            target_owner,
                        ),
                    });
                }
                Ok(loans)
            }
        }
    }

    /// Returns the field path to the type's sole reference-bearing position when
    /// it has exactly one reachable purely through record fields, else `None`
    /// (zero, several, or references reachable only through an array element or
    /// enum payload, which are not field-addressable places). Used to attribute
    /// a call result's carried loans at field granularity. See the default arm
    /// of [`Self::collect_value_loans_for_store`].
    fn single_reference_field_path(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<Option<Vec<String>>> {
        let mut paths = Vec::new();
        let mut prefix = Vec::new();
        let mut resolvable = true;
        self.collect_reference_field_paths(
            root,
            type_hash,
            &mut prefix,
            &mut paths,
            &mut resolvable,
        )?;
        if resolvable && paths.len() == 1 {
            Ok(paths.into_iter().next())
        } else {
            Ok(None)
        }
    }

    fn collect_reference_field_paths(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        prefix: &mut Vec<String>,
        paths: &mut Vec<Vec<String>>,
        resolvable: &mut bool,
    ) -> Result<()> {
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Reference { .. } | TypeSpec::Slice { .. } => paths.push(prefix.clone()),
            TypeSpec::Record(fields) => {
                for field in fields {
                    if self
                        .value_class_in_root(root, &field.type_hash)?
                        .contains_reference
                    {
                        prefix.push(field.name.clone());
                        self.collect_reference_field_paths(
                            root,
                            &field.type_hash,
                            prefix,
                            paths,
                            resolvable,
                        )?;
                        prefix.pop();
                    }
                }
            }
            TypeSpec::Enum(variants) => {
                for variant in variants {
                    if self
                        .value_class_in_root(root, &variant.type_hash)?
                        .contains_reference
                    {
                        // References in an enum payload are not field-addressable.
                        *resolvable = false;
                    }
                }
            }
            TypeSpec::FixedArray { element, .. } => {
                if self.value_class_in_root(root, &element)?.contains_reference {
                    // References in array elements are not field-addressable.
                    *resolvable = false;
                }
            }
            TypeSpec::Box { element } => {
                if self.value_class_in_root(root, &element)?.contains_reference {
                    // References inside boxes are owned behind an indirection and
                    // not field-addressable from the containing record.
                    *resolvable = false;
                }
            }
            TypeSpec::Vec { element } => {
                if self.value_class_in_root(root, &element)?.contains_reference {
                    *resolvable = false;
                }
            }
            TypeSpec::Builtin(_)
            | TypeSpec::RawPointer { .. }
            | TypeSpec::String
            | TypeSpec::TypeParam { .. }
            | TypeSpec::Named { .. } => {}
        }
        Ok(())
    }

    fn check_loans_point_to_live_storage(
        &self,
        loans: &[ActiveLoan],
        state: &MoveBorrowState,
    ) -> Result<()> {
        for loan in loans {
            if let LoanRoot::Local(local_id) = loan.place.root
                && !state.locals.contains(&local_id)
            {
                bail!(
                    "loan of {:?} outlives local storage {:?}",
                    loan.place,
                    local_id
                );
            }
        }
        Ok(())
    }

    fn check_no_loans_outlive_local(&self, local_id: usize, state: &MoveBorrowState) -> Result<()> {
        for loan in &state.active {
            if matches!(loan.place.root, LoanRoot::Local(id) if id == local_id) {
                bail!(
                    "loan of {:?} outlives local storage {:?}",
                    loan.place,
                    local_id
                );
            }
        }
        Ok(())
    }

    fn verify_place_value_use(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        state: &mut MoveBorrowState,
    ) -> Result<()> {
        let place = self.loan_place_for_expr(expr_hash, param_types, &state.locals)?;
        self.check_place_not_moved(&place, state)?;
        let type_hash = self.expr_declared_type(expr_hash)?;
        let class = self.value_class_in_root(root, &type_hash)?;
        if class.copy_kind == ValueCopyKind::MoveOnly {
            // Granular drop glue (SPEC_V3 §7): a partial move out of a record field
            // or a CONSTANT-index array element (`[N]`) is supported — lowering drops
            // the live remainder of the enclosing aggregate (sibling fields / other
            // elements by index) while skipping the moved-out sub-place. A DYNAMIC
            // index (`[*]`) names an unknown element, so the static scaffold cannot
            // tell which element survived; it stays fail-closed (would need a runtime
            // drop flag the design forbids).
            if place.fields.iter().any(|segment| segment == "[*]") {
                bail!(
                    "unsupported_move: partial move of an owned array element at a dynamic index ({:?}); only constant-index element moves are supported (SPEC_V3 §7)",
                    place
                );
            }
            // A partial move of a field/element reached through a `box` auto-deref
            // (`h.inner` where `h: box<Holder>`) IS field-granular-droppable: lowering
            // records a `Deref` step (the deref is flattened away in this checker
            // `place`, but a `box` is a UNIQUE owner so the path identity is still
            // sound — no aliasing, unlike a `&mut`/raw-pointer deref), drops the live
            // pointee siblings through the deref, and frees the box shell separately
            // (SPEC_V3 §7). The conflict-tracking treats `h.inner` like a direct field
            // — correct, since the box owns exactly one pointee. (A move out of
            // *borrowed* box content is still rejected by the move-out-of-loan check.)
            self.check_move_conflicts(&place, &state.active)?;
            // Inside a `loop`/`fold` (#14a → loop-carried drop glue): the move
            // repeats once per iteration, so it is legal only for per-iteration
            // storage — or, wholly, for the loop accumulator the back-edge
            // refills. Checked here at the recording site, because scope pops
            // retire `moved` entries and would hide a per-iteration move from
            // any after-the-fact diff.
            if let Some(scope) = state.iteration_scope {
                let allowed = match place.root {
                    LoanRoot::Local(id) if Some(id) == scope.movable_whole => {
                        if !place.fields.is_empty() {
                            bail!(
                                "unsupported_move: {} may consume the loop accumulator only as a whole; a partial accumulator move is not supported",
                                scope.construct
                            );
                        }
                        true
                    }
                    LoanRoot::Local(id) => id >= scope.floor,
                    LoanRoot::Param(_) | LoanRoot::Static(_) => false,
                };
                if !allowed {
                    bail!(
                        "unsupported_move: {} cannot move owned values that outlive one iteration (the move would repeat); only per-iteration locals{} may move",
                        scope.construct,
                        if scope.movable_whole.is_some() {
                            " and the whole accumulator"
                        } else {
                            ""
                        }
                    );
                }
            }
            state.moved.push(place);
        } else {
            self.check_shared_read_conflicts(&place, &state.active)?;
        }
        Ok(())
    }

    fn move_source_places_for_expr(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        locals: &[usize],
    ) -> Result<Vec<LoanPlace>> {
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("param_ref" | "local_ref" | "field_access" | "array_index") => {
                let type_hash = self.expr_declared_type(expr_hash)?;
                let class = self.value_class_in_root(root, &type_hash)?;
                if class.copy_kind == ValueCopyKind::MoveOnly {
                    Ok(vec![self.loan_place_for_expr(
                        expr_hash,
                        param_types,
                        locals,
                    )?])
                } else {
                    Ok(Vec::new())
                }
            }
            Some("let") => {
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing body"))?;
                let mut nested_locals = locals.to_vec();
                let synthetic_local = synthetic_let_local_id(&nested_locals);
                nested_locals.push(synthetic_local);
                let mut sources =
                    self.move_source_places_for_expr(root, body_hash, param_types, &nested_locals)?;
                sources.retain(
                    |source| !matches!(source.root, LoanRoot::Local(id) if id == synthetic_local),
                );
                Ok(sources)
            }
            Some("if") => {
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                let mut sources =
                    self.move_source_places_for_expr(root, then_hash, param_types, locals)?;
                for source in
                    self.move_source_places_for_expr(root, else_hash, param_types, locals)?
                {
                    if !sources.contains(&source) {
                        sources.push(source);
                    }
                }
                Ok(sources)
            }
            // Constructors move their move-only operands into the new value, so
            // they must report those operands as move sources — otherwise the
            // enclosing binding never retires the source binding's loan and the
            // freshly-attributed loan spuriously conflicts with it. Keep this in
            // lock-step with the `record_literal`/`enum_construct` arms of
            // `collect_value_loans_for_store`.
            Some("record_literal") => {
                let mut sources = Vec::new();
                for field in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("record_literal missing fields"))?
                {
                    let value = field
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing value"))?;
                    for source in
                        self.move_source_places_for_expr(root, value, param_types, locals)?
                    {
                        if !sources.contains(&source) {
                            sources.push(source);
                        }
                    }
                }
                Ok(sources)
            }
            Some("array_literal") => {
                let mut sources = Vec::new();
                for element in payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?
                {
                    let value = element
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing value"))?;
                    for source in
                        self.move_source_places_for_expr(root, value, param_types, locals)?
                    {
                        if !sources.contains(&source) {
                            sources.push(source);
                        }
                    }
                }
                Ok(sources)
            }
            Some("array_fill") => {
                // The fill copies a Copy value into every slot; the only moves are
                // whatever evaluating `value` once moves.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                self.move_source_places_for_expr(root, value, param_types, locals)
            }
            Some("enum_construct") => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                self.move_source_places_for_expr(root, value, param_types, locals)
            }
            Some("box_new") => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("box_new missing value"))?;
                self.move_source_places_for_expr(root, value, param_types, locals)
            }
            Some("unbox") => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unbox missing value"))?;
                self.move_source_places_for_expr(root, value, param_types, locals)
            }
            Some("int_cast") => {
                // A cast does not move its (Copy scalar) operand, but the operand
                // sub-expression might (e.g. `to_u32(unbox(b))`); propagate it.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing value"))?;
                self.move_source_places_for_expr(root, value, param_types, locals)
            }
            Some("subslice") => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing target"))?;
                self.move_source_places_for_expr(root, target, param_types, locals)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn source_place_for_value_expr(
        &self,
        expr_hash: &str,
        param_types: &[String],
        locals: &[usize],
    ) -> Result<Option<LoanPlace>> {
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("param_ref" | "local_ref" | "field_access" | "array_index") => Ok(Some(
                self.loan_place_for_expr(expr_hash, param_types, locals)?,
            )),
            Some("let") => {
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing body"))?;
                let mut nested_locals = locals.to_vec();
                let synthetic_local = synthetic_let_local_id(&nested_locals);
                nested_locals.push(synthetic_local);
                let source =
                    self.source_place_for_value_expr(body_hash, param_types, &nested_locals)?;
                let Some(source) = source else {
                    return Ok(None);
                };
                if matches!(source.root, LoanRoot::Local(id) if id == synthetic_local) {
                    Ok(None)
                } else {
                    Ok(Some(source))
                }
            }
            Some("subslice") => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("subslice missing target"))?;
                self.source_place_for_value_expr(target, param_types, locals)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn expr_declared_type(&self, expr_hash: &str) -> Result<String> {
        self.get_payload(expr_hash)?
            .get("type")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("expression missing type {expr_hash}"))
    }

    fn check_place_not_moved(&self, place: &LoanPlace, state: &MoveBorrowState) -> Result<()> {
        for moved in &state.moved {
            if places_overlap(place, moved) {
                bail!(
                    "bad_move: use after move of {:?}; attempted to use {:?}",
                    moved,
                    place
                );
            }
        }
        Ok(())
    }

    /// Not-moved check parameterized by how the place is used. A `Value` or
    /// whole-`Place` use needs the entire place — every sub-place — live; a
    /// `ProjectionBase` use only needs the place and its ancestors live, because
    /// a moved-out *sibling* field does not stop us narrowing into a live field
    /// (field-granular drop glue, SPEC_V3 §7).
    fn check_place_not_moved_for_use(
        &self,
        place: &LoanPlace,
        state: &MoveBorrowState,
        expr_use: ExprUse,
    ) -> Result<()> {
        match expr_use {
            ExprUse::Value | ExprUse::Place => self.check_place_not_moved(place, state),
            ExprUse::ProjectionBase => {
                for moved in &state.moved {
                    if moved.root == place.root && fields_prefix(&moved.fields, &place.fields) {
                        bail!(
                            "bad_move: use after move of {:?}; attempted to project into {:?}",
                            moved,
                            place
                        );
                    }
                }
                Ok(())
            }
        }
    }

    /// Map a borrow-target expression to the `LoanPlace` (root slot + field
    /// path) it names.
    ///
    /// LOAD-BEARING INVARIANT for exclusivity: `places_overlap` decides loan
    /// conflicts purely structurally, so two `LoanPlace`s denoting the SAME
    /// storage must compare as overlapping.
    ///
    /// Note this is NOT guaranteed by the absence of a deref place form: a borrow
    /// target CAN traverse a reference field — e.g. `&mut editor.line.x` where
    /// `editor.line: &mut Line` is accepted, because the `field_access` arm
    /// recurses through its target unconditionally. The resulting loan place is
    /// the syntactic path (`{editor,[line,x]}`), NOT the pointee's identity, so it
    /// does NOT structurally overlap a loan on the pointee (`{line,[]}`).
    /// Exclusivity nonetheless holds today because of affine `&mut` uniqueness:
    /// building `editor` consumed a `&mut line`, leaving a covering loan over all
    /// of `line` that blocks every DIRECT path to `line.x`, while the only other
    /// path (`editor.line.x`) has a single self-conflicting loan identity — so two
    /// live aliasing `&mut` cannot be constructed. `param_types` is unused because
    /// the path, not the pointee type, is what is compared.
    ///
    /// If a place form is ever added that reaches a pointee WITHOUT keeping the
    /// originating `&mut`'s covering loan live (a reborrow that splits a loan,
    /// moving a `&mut` field out of its record, partial/field-granular loans, or a
    /// raw-pointer deref), this path-based identity becomes unsound and aliasing
    /// `&mut` would be accepted. Such a form MUST resolve to the pointee's
    /// recorded loan identity (not the syntactic path) instead.
    fn loan_place_for_expr(
        &self,
        expr_hash: &str,
        param_types: &[String],
        locals: &[usize],
    ) -> Result<LoanPlace> {
        let _ = param_types;
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
                Ok(LoanPlace {
                    root: LoanRoot::Param(index),
                    fields: Vec::new(),
                })
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                let local_id = local_usize_at_depth(locals, depth)
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))?;
                Ok(LoanPlace {
                    root: LoanRoot::Local(local_id),
                    fields: Vec::new(),
                })
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
                let mut place = self.loan_place_for_expr(target, param_types, locals)?;
                place.fields.push(field.to_string());
                Ok(place)
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
                let mut place = self.loan_place_for_expr(target, param_types, locals)?;
                place.fields.push(self.array_index_place_segment(index)?);
                Ok(place)
            }
            "static_bytes" => {
                let static_data = payload
                    .get("static_data")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing static_data"))?;
                Ok(LoanPlace {
                    root: LoanRoot::Static(static_data.to_string()),
                    fields: Vec::new(),
                })
            }
            other => bail!("borrow target {other} is not an addressable place"),
        }
    }

    fn add_checked_value_loans(
        &self,
        active: &mut Vec<ActiveLoan>,
        loans: &[ActiveLoan],
    ) -> Result<Vec<ActiveLoan>> {
        let mut added = Vec::new();
        for loan in loans {
            self.check_loan_conflicts(&loan.kind, &loan.place, loan.exclusive, active)?;
            if active.contains(loan) {
                continue;
            }
            active.push(loan.clone());
            added.push(loan.clone());
        }
        Ok(added)
    }

    // Aliasing/exclusivity checks below only consider `exclusive` loans (those
    // derived from safe references/slices). Non-exclusive raw-pointer loans are
    // tracked for liveness/escape only and may legally alias under `unsafe`
    // (SPEC §15), so they neither conflict with nor are blocked by other loans.
    fn check_loan_conflicts(
        &self,
        kind: &LoanKind,
        place: &LoanPlace,
        new_exclusive: bool,
        active: &[ActiveLoan],
    ) -> Result<()> {
        if !new_exclusive {
            return Ok(());
        }
        for loan in active {
            if loan.exclusive
                && places_overlap(place, &loan.place)
                && (*kind == LoanKind::Mutable || loan.kind == LoanKind::Mutable)
            {
                bail!(
                    "exclusive loan conflict: {:?} borrow of {:?} conflicts with live {:?} borrow of {:?}",
                    kind,
                    place,
                    loan.kind,
                    loan.place
                );
            }
        }
        Ok(())
    }

    fn check_shared_read_conflicts(&self, place: &LoanPlace, active: &[ActiveLoan]) -> Result<()> {
        for loan in active {
            if loan.exclusive
                && loan.kind == LoanKind::Mutable
                && places_overlap(place, &loan.place)
            {
                bail!(
                    "shared read of {:?} conflicts with live mutable borrow of {:?}",
                    place,
                    loan.place
                );
            }
        }
        Ok(())
    }

    fn check_move_conflicts(&self, place: &LoanPlace, active: &[ActiveLoan]) -> Result<()> {
        for loan in active {
            if loan.exclusive && places_overlap(place, &loan.place) {
                bail!(
                    "move of {:?} conflicts with live {:?} borrow of {:?}",
                    place,
                    loan.kind,
                    loan.place
                );
            }
        }
        Ok(())
    }

    pub(crate) fn expr_requires_state(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        Ok(
            match payload
                .get("expr_kind")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
            {
                "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" | "param_ref"
                | "local_ref" => false,
                "arg_count" => false,
                "arg_len" => self.expr_child_requires_state(&payload, "index")?,
                "arg_byte" => {
                    self.expr_child_requires_state(&payload, "index")?
                        || self.expr_child_requires_state(&payload, "byte")?
                }
                "assign" => true,
                "call" => {
                    let mut required = false;
                    for arg in payload
                        .get("args")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("call missing args"))?
                    {
                        let arg = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        required |= self.expr_requires_state(arg)?;
                    }
                    required
                }
                "binary" => {
                    self.expr_child_requires_state(&payload, "left")?
                        || self.expr_child_requires_state(&payload, "right")?
                }
                "unary" => self.expr_child_requires_state(&payload, "expr")?,
                "borrow_shared" | "borrow_mut" => {
                    self.expr_child_requires_state(&payload, "target")?
                }
                "slice_from_array" | "slice_len" => {
                    self.expr_child_requires_state(&payload, "target")?
                }
                "subslice" => {
                    self.expr_child_requires_state(&payload, "target")?
                        || self.expr_child_requires_state(&payload, "start")?
                        || self.expr_child_requires_state(&payload, "len")?
                }
                "box_new" => self.expr_child_requires_state(&payload, "value")?,
                "unbox" => self.expr_child_requires_state(&payload, "value")?,
                "vec_new" => self.expr_child_requires_state(&payload, "capacity")?,
                "vec_push" => true,
                "vec_get" => {
                    self.expr_child_requires_state(&payload, "target")?
                        || self.expr_child_requires_state(&payload, "index")?
                }
                "vec_len" => self.expr_child_requires_state(&payload, "target")?,
                "string_new" => self.expr_child_requires_state(&payload, "source")?,
                "string_len" => self.expr_child_requires_state(&payload, "target")?,
                "string_with_capacity" => {
                    self.expr_child_requires_state(&payload, "capacity")?
                }
                "string_push" => true,
                "string_set" => true,
                "string_get" => {
                    self.expr_child_requires_state(&payload, "target")?
                        || self.expr_child_requires_state(&payload, "index")?
                }
                "raw_ptr_cast" | "int_cast" => self.expr_child_requires_state(&payload, "value")?,
                "raw_load" => self.expr_child_requires_state(&payload, "pointer")?,
                "raw_store" => true,
                "let" => {
                    self.expr_child_requires_state(&payload, "value")?
                        || self.expr_child_requires_state(&payload, "body")?
                }
                "if" => {
                    self.expr_child_requires_state(&payload, "cond")?
                        || self.expr_child_requires_state(&payload, "then")?
                        || self.expr_child_requires_state(&payload, "else")?
                }
                "return" => self.expr_child_requires_state(&payload, "value")?,
                "fold" => {
                    self.expr_child_requires_state(&payload, "target")?
                        || self.expr_child_requires_state(&payload, "init")?
                        || self.expr_child_requires_state(&payload, "body")?
                }
                "loop" => {
                    self.expr_child_requires_state(&payload, "init")?
                        || self.expr_child_requires_state(&payload, "cond")?
                        || self.expr_child_requires_state(&payload, "body")?
                }
                "record_literal" => {
                    let mut required = false;
                    for field in payload
                        .get("fields")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("record_literal missing fields"))?
                    {
                        let value = field
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing value"))?;
                        required |= self.expr_requires_state(value)?;
                    }
                    required
                }
                "array_literal" => {
                    let mut required = false;
                    for element in payload
                        .get("elements")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("array_literal missing elements"))?
                    {
                        let value = element
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array element missing value"))?;
                        required |= self.expr_requires_state(value)?;
                    }
                    required
                }
                "array_fill" => self.expr_child_requires_state(&payload, "value")?,
                "array_set" => {
                    self.expr_child_requires_state(&payload, "array")?
                        || self.expr_child_requires_state(&payload, "index")?
                        || self.expr_child_requires_state(&payload, "value")?
                }
                "array_index" => {
                    self.expr_child_requires_state(&payload, "target")?
                        || self.expr_child_requires_state(&payload, "index")?
                }
                "field_access" => self.expr_child_requires_state(&payload, "target")?,
                "enum_construct" => self.expr_child_requires_state(&payload, "value")?,
                "case" => {
                    let mut required = self.expr_child_requires_state(&payload, "expr")?;
                    for arm in payload
                        .get("arms")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("case missing arms"))?
                    {
                        // A guard (R14) is evaluated at runtime, so an inline effect
                        // inside it requires the enclosing function to declare that
                        // effect (call-borne effects flow through the dependency graph).
                        if let Some(guard) = arm.get("guard").and_then(JsonValue::as_str) {
                            required |= self.expr_requires_state(guard)?;
                        }
                        let body = arm
                            .get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?;
                        required |= self.expr_requires_state(body)?;
                    }
                    required
                }
                other => bail!("unknown expression kind {other}"),
            },
        )
    }

    /// Whether the expression reads the process's command-line arguments (R12)
    /// — the intrinsic `io` requirement. Unlike the state/alloc/unsafe walkers,
    /// only the three `arg_*` builtins require `io` intrinsically (extern io
    /// propagates by callee signature, not by expression kind), so this walk is
    /// derived from the central child table instead of a per-kind match — a new
    /// expression kind is covered automatically.
    pub(crate) fn expr_requires_io(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        if matches!(
            payload.get("expr_kind").and_then(JsonValue::as_str),
            Some("arg_count" | "arg_len" | "arg_byte")
        ) {
            return Ok(true);
        }
        let mut requires = false;
        for_each_child_expr_hash(&payload, &mut |child| {
            if !requires {
                requires = self.expr_requires_io(child)?;
            }
            Ok(())
        })?;
        Ok(requires)
    }

    fn expr_child_requires_state(&self, payload: &JsonValue, key: &str) -> Result<bool> {
        let child = payload
            .get(key)
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing {key}"))?;
        self.expr_requires_state(child)
    }

    pub(crate) fn expr_requires_alloc(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        Ok(
            match payload
                .get("expr_kind")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
            {
                "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" | "param_ref"
                | "local_ref" => false,
                "arg_count" => false,
                "arg_len" => self.expr_child_requires_alloc(&payload, "index")?,
                "arg_byte" => {
                    self.expr_child_requires_alloc(&payload, "index")?
                        || self.expr_child_requires_alloc(&payload, "byte")?
                }
                // `unbox` frees the box shell, so it requires `alloc` just like the
                // allocating builtins (the deallocation is in the alloc effect domain).
                "box_new" | "unbox" | "vec_new" | "string_new" | "string_with_capacity" => true,
                "assign" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "value")?
                }
                "call" => {
                    let mut required = false;
                    for arg in payload
                        .get("args")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("call missing args"))?
                    {
                        let arg = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        required |= self.expr_requires_alloc(arg)?;
                    }
                    required
                }
                "binary" => {
                    self.expr_child_requires_alloc(&payload, "left")?
                        || self.expr_child_requires_alloc(&payload, "right")?
                }
                "unary" => self.expr_child_requires_alloc(&payload, "expr")?,
                "borrow_shared" | "borrow_mut" | "slice_from_array" | "slice_len" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                }
                "raw_ptr_cast" | "int_cast" => self.expr_child_requires_alloc(&payload, "value")?,
                "raw_load" => self.expr_child_requires_alloc(&payload, "pointer")?,
                "raw_store" => {
                    self.expr_child_requires_alloc(&payload, "pointer")?
                        || self.expr_child_requires_alloc(&payload, "value")?
                }
                "subslice" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "start")?
                        || self.expr_child_requires_alloc(&payload, "len")?
                }
                "vec_push" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "value")?
                }
                "vec_get" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "index")?
                }
                "vec_len" => self.expr_child_requires_alloc(&payload, "target")?,
                "string_len" => self.expr_child_requires_alloc(&payload, "target")?,
                "string_push" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "value")?
                }
                "string_get" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "index")?
                }
                "string_set" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "index")?
                        || self.expr_child_requires_alloc(&payload, "value")?
                }
                "let" => {
                    self.expr_child_requires_alloc(&payload, "value")?
                        || self.expr_child_requires_alloc(&payload, "body")?
                }
                "if" => {
                    self.expr_child_requires_alloc(&payload, "cond")?
                        || self.expr_child_requires_alloc(&payload, "then")?
                        || self.expr_child_requires_alloc(&payload, "else")?
                }
                "return" => self.expr_child_requires_alloc(&payload, "value")?,
                "fold" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "init")?
                        || self.expr_child_requires_alloc(&payload, "body")?
                }
                "loop" => {
                    self.expr_child_requires_alloc(&payload, "init")?
                        || self.expr_child_requires_alloc(&payload, "cond")?
                        || self.expr_child_requires_alloc(&payload, "body")?
                }
                "record_literal" => {
                    let mut required = false;
                    for field in payload
                        .get("fields")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("record_literal missing fields"))?
                    {
                        let value = field
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing value"))?;
                        required |= self.expr_requires_alloc(value)?;
                    }
                    required
                }
                "array_literal" => {
                    let mut required = false;
                    for element in payload
                        .get("elements")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("array_literal missing elements"))?
                    {
                        let value = element
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array element missing value"))?;
                        required |= self.expr_requires_alloc(value)?;
                    }
                    required
                }
                "array_fill" => self.expr_child_requires_alloc(&payload, "value")?,
                "array_set" => {
                    self.expr_child_requires_alloc(&payload, "array")?
                        || self.expr_child_requires_alloc(&payload, "index")?
                        || self.expr_child_requires_alloc(&payload, "value")?
                }
                "array_index" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "index")?
                }
                "field_access" => self.expr_child_requires_alloc(&payload, "target")?,
                "enum_construct" => self.expr_child_requires_alloc(&payload, "value")?,
                "case" => {
                    let mut required = self.expr_child_requires_alloc(&payload, "expr")?;
                    for arm in payload
                        .get("arms")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("case missing arms"))?
                    {
                        if let Some(guard) = arm.get("guard").and_then(JsonValue::as_str) {
                            required |= self.expr_requires_alloc(guard)?;
                        }
                        let body = arm
                            .get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?;
                        required |= self.expr_requires_alloc(body)?;
                    }
                    required
                }
                other => bail!("unknown expression kind {other}"),
            },
        )
    }

    fn expr_child_requires_alloc(&self, payload: &JsonValue, key: &str) -> Result<bool> {
        let child = payload
            .get(key)
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing {key}"))?;
        self.expr_requires_alloc(child)
    }

    pub(crate) fn expr_requires_unsafe(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        Ok(
            match payload
                .get("expr_kind")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
            {
                "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" | "param_ref"
                | "local_ref" => false,
                "arg_count" => false,
                "arg_len" => self.expr_child_requires_unsafe(&payload, "index")?,
                "arg_byte" => {
                    self.expr_child_requires_unsafe(&payload, "index")?
                        || self.expr_child_requires_unsafe(&payload, "byte")?
                }
                "raw_ptr_cast" | "raw_load" | "raw_store" => true,
                "assign" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "value")?
                }
                "call" => {
                    let mut required = false;
                    for arg in payload
                        .get("args")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("call missing args"))?
                    {
                        let arg = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        required |= self.expr_requires_unsafe(arg)?;
                    }
                    required
                }
                "binary" => {
                    self.expr_child_requires_unsafe(&payload, "left")?
                        || self.expr_child_requires_unsafe(&payload, "right")?
                }
                "unary" => self.expr_child_requires_unsafe(&payload, "expr")?,
                "borrow_shared" | "borrow_mut" | "slice_from_array" | "slice_len" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                }
                "subslice" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "start")?
                        || self.expr_child_requires_unsafe(&payload, "len")?
                }
                "box_new" => self.expr_child_requires_unsafe(&payload, "value")?,
                "unbox" => self.expr_child_requires_unsafe(&payload, "value")?,
                "int_cast" => self.expr_child_requires_unsafe(&payload, "value")?,
                "vec_new" => self.expr_child_requires_unsafe(&payload, "capacity")?,
                "vec_push" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "value")?
                }
                "vec_get" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "index")?
                }
                "vec_len" => self.expr_child_requires_unsafe(&payload, "target")?,
                "string_new" => self.expr_child_requires_unsafe(&payload, "source")?,
                "string_len" => self.expr_child_requires_unsafe(&payload, "target")?,
                "string_with_capacity" => {
                    self.expr_child_requires_unsafe(&payload, "capacity")?
                }
                "string_push" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "value")?
                }
                "string_get" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "index")?
                }
                "string_set" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "index")?
                        || self.expr_child_requires_unsafe(&payload, "value")?
                }
                "let" => {
                    self.expr_child_requires_unsafe(&payload, "value")?
                        || self.expr_child_requires_unsafe(&payload, "body")?
                }
                "if" => {
                    self.expr_child_requires_unsafe(&payload, "cond")?
                        || self.expr_child_requires_unsafe(&payload, "then")?
                        || self.expr_child_requires_unsafe(&payload, "else")?
                }
                "return" => self.expr_child_requires_unsafe(&payload, "value")?,
                "fold" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "init")?
                        || self.expr_child_requires_unsafe(&payload, "body")?
                }
                "loop" => {
                    self.expr_child_requires_unsafe(&payload, "init")?
                        || self.expr_child_requires_unsafe(&payload, "cond")?
                        || self.expr_child_requires_unsafe(&payload, "body")?
                }
                "record_literal" => {
                    let mut required = false;
                    for field in payload
                        .get("fields")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("record_literal missing fields"))?
                    {
                        let value = field
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("record field missing value"))?;
                        required |= self.expr_requires_unsafe(value)?;
                    }
                    required
                }
                "array_literal" => {
                    let mut required = false;
                    for element in payload
                        .get("elements")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("array_literal missing elements"))?
                    {
                        let value = element
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("array element missing value"))?;
                        required |= self.expr_requires_unsafe(value)?;
                    }
                    required
                }
                "array_fill" => self.expr_child_requires_unsafe(&payload, "value")?,
                "array_set" => {
                    self.expr_child_requires_unsafe(&payload, "array")?
                        || self.expr_child_requires_unsafe(&payload, "index")?
                        || self.expr_child_requires_unsafe(&payload, "value")?
                }
                "array_index" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "index")?
                }
                "field_access" => self.expr_child_requires_unsafe(&payload, "target")?,
                "enum_construct" => self.expr_child_requires_unsafe(&payload, "value")?,
                "case" => {
                    let mut required = self.expr_child_requires_unsafe(&payload, "expr")?;
                    for arm in payload
                        .get("arms")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| anyhow!("case missing arms"))?
                    {
                        if let Some(guard) = arm.get("guard").and_then(JsonValue::as_str) {
                            required |= self.expr_requires_unsafe(guard)?;
                        }
                        let body = arm
                            .get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?;
                        required |= self.expr_requires_unsafe(body)?;
                    }
                    required
                }
                other => bail!("unknown expression kind {other}"),
            },
        )
    }

    fn expr_child_requires_unsafe(&self, payload: &JsonValue, key: &str) -> Result<bool> {
        let child = payload
            .get(key)
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing {key}"))?;
        self.expr_requires_unsafe(child)
    }

    pub(crate) fn verify_expr_type(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        param_types: &[String],
        allowed_regions: &BTreeSet<String>,
        fn_return: Option<&str>,
    ) -> Result<String> {
        self.verify_expr_type_with_locals(
            expr_hash,
            root,
            param_types,
            allowed_regions,
            &mut Vec::new(),
            fn_return,
        )
    }

    /// Re-verify a typed nested destructuring payload pattern node (R14) against the
    /// type it matches (`match_type`, an enum), returning the chain's leaf binding
    /// `(name, type)` to scope for the arm body. Node shape mirrors the type-check
    /// encoding: `{"variant": v, "binding_name"?: x, "payload_pattern"?: {..}}`.
    fn verify_payload_pattern(
        &self,
        root: &ProgramRootPayload,
        node: &JsonValue,
        match_type: &str,
    ) -> Result<Option<(String, String)>> {
        let TypeSpec::Enum(variants) = self.type_spec_in_root(root, match_type)? else {
            bail!("nested pattern requires an enum payload");
        };
        let variant = node
            .get("variant")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("nested pattern node missing variant"))?;
        validate_projection_identifier("enum variant", variant)?;
        let payload_type = variants
            .iter()
            .find(|candidate| candidate.name == variant)
            .map(|candidate| candidate.type_hash.clone())
            .ok_or_else(|| anyhow!("nested pattern uses unknown variant {variant}"))?;
        if let Some(name) = node.get("binding_name").and_then(JsonValue::as_str) {
            validate_projection_identifier("case binding", name)?;
            Ok(Some((name.to_string(), payload_type)))
        } else if let Some(inner) = node.get("payload_pattern") {
            self.verify_payload_pattern(root, inner, &payload_type)
        } else {
            Ok(None)
        }
    }

    /// Re-verify a scalar literal `case` (R14) typed node: arms are literal
    /// patterns (`i64`/`bool`) plus an optional `_` wildcard, body types unify to
    /// the declared type, and the case is exhaustive.
    #[allow(clippy::too_many_arguments)]
    fn verify_scalar_case_type(
        &self,
        scrutinee_type: &str,
        arms: &[JsonValue],
        declared_type: &str,
        root: &ProgramRootPayload,
        param_types: &[String],
        allowed_regions: &BTreeSet<String>,
        locals: &mut Vec<String>,
        fn_return: Option<&str>,
    ) -> Result<String> {
        let is_i64 = scrutinee_type == type_hash_for("I64");
        let mut result_type: Option<String> = None;
        // First arm's body type, used only when every arm diverges (R7).
        let mut fallback_type: Option<String> = None;
        let mut has_default = false;
        let mut seen_i64: BTreeSet<String> = BTreeSet::new();
        let mut seen_bool: BTreeSet<bool> = BTreeSet::new();
        for (index, arm) in arms.iter().enumerate() {
            if arm.get("binding_name").is_some() {
                bail!("scalar case arm cannot bind a value");
            }
            if arm.get("guard").is_some() && !is_i64 {
                bail!("if-guards require an i64 scrutinee");
            }
            let guarded = arm.get("guard").is_some();
            if arm.get("default").and_then(JsonValue::as_bool) == Some(true) {
                // A guarded wildcard is conditional; only the UNGUARDED catch-all must
                // be unique, last, and prove exhaustiveness (R14).
                if !guarded {
                    if index + 1 != arms.len() {
                        bail!("default case arm must be last");
                    }
                    if has_default {
                        bail!("duplicate default case arm");
                    }
                    has_default = true;
                }
            } else if arm.get("range_lo").is_some() {
                // i64 range pattern (R14). Overlaps are intentionally not flagged
                // (first-match semantics); only well-formedness is re-checked here.
                if !is_i64 {
                    bail!("range case patterns require an i64 scrutinee");
                }
                let bound = |key: &str| -> Result<i64> {
                    arm.get(key)
                        .and_then(JsonValue::as_str)
                        .and_then(|value| value.parse::<i64>().ok())
                        .ok_or_else(|| anyhow!("range case bound {key} must be an integer"))
                };
                let lo = bound("range_lo")?;
                let hi = bound("range_hi")?;
                let inclusive = arm
                    .get("range_inclusive")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let empty = if inclusive { lo > hi } else { lo >= hi };
                if empty {
                    bail!("empty range case pattern");
                }
            } else if is_i64 {
                let value = arm
                    .get("literal_i64")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("scalar i64 case arm must be an integer literal or `_`"))?;
                value
                    .parse::<i64>()
                    .with_context(|| format!("invalid i64 case pattern {value}"))?;
                // Only an UNGUARDED literal fully covers its value (R14); a guarded one
                // may repeat. Overlaps are first-match (not flagged).
                if !guarded && !seen_i64.insert(value.to_string()) {
                    bail!("duplicate case pattern {value}");
                }
            } else {
                let value = arm
                    .get("literal_bool")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("scalar bool case arm must be a bool literal or `_`"))?;
                if !seen_bool.insert(value) {
                    bail!("duplicate case pattern {value}");
                }
            }
            // Re-verify the `if` guard (R14): a `bool` predicate in the arm's scope
            // (mirrors type_scalar_case). Its effects are accounted by the effect
            // checker; the no-move discipline is enforced at lowering, which `verify`
            // also runs.
            if let Some(guard_hash) = arm.get("guard").and_then(JsonValue::as_str) {
                let guard_type = self.verify_expr_type_with_locals(
                    guard_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if guard_type != type_hash_for("Bool") {
                    bail!("case guard must be a bool expression");
                }
            }
            let body_hash = arm
                .get("body")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("case arm missing body"))?;
            let body_type = self.verify_expr_type_with_locals(
                body_hash,
                root,
                param_types,
                allowed_regions,
                locals,
                fn_return,
            )?;
            if fallback_type.is_none() {
                fallback_type = Some(body_type.clone());
            }
            // Early exit (R7): a divergent arm neither fixes nor must match the type.
            if !self.typed_expr_diverges(body_hash)? {
                match &result_type {
                    Some(expected) if expected != &body_type => bail!("case arm type mismatch"),
                    Some(_) => {}
                    None => result_type = Some(body_type),
                }
            }
        }
        let exhaustive = has_default
            || (!is_i64 && seen_bool.contains(&true) && seen_bool.contains(&false));
        if !exhaustive {
            bail!("case expression is not exhaustive: a scalar `case` needs a `_` wildcard");
        }
        let actual_type = result_type
            .or(fallback_type)
            .ok_or_else(|| anyhow!("case expression has no arms"))?;
        if declared_type != actual_type {
            bail!("bad_type: case declares {declared_type}, actual {actual_type}");
        }
        Ok(actual_type)
    }

    fn verify_expr_type_with_locals(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        param_types: &[String],
        allowed_regions: &BTreeSet<String>,
        locals: &mut Vec<String>,
        fn_return: Option<&str>,
    ) -> Result<String> {
        if self.get_kind(expr_hash)? != "Expression" {
            bail!("bad_type: object is not expression {expr_hash}");
        }
        let payload = self.get_payload(expr_hash)?;
        let declared_type = payload
            .get("type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing type {expr_hash}"))?
            .to_string();
        let actual_type = match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => {
                // An integer literal's width is carried by its declared `type`
                // (context-typed; R5). Re-derive it independently: the declared type
                // must be a sized integer and the value must fit it.
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?;
                let declared = payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing type"))?;
                let TypeSpec::Builtin(name) = self.type_spec(declared)? else {
                    bail!("integer literal declares a non-builtin type");
                };
                let int = scalar_int_type(&name)
                    .ok_or_else(|| anyhow!("integer literal declares non-integer type {name}"))?;
                if !int_literal_in_range(value, int) {
                    bail!(
                        "integer literal {value} is out of range for {}",
                        int.name.to_ascii_lowercase()
                    );
                }
                declared.to_string()
            }
            "literal_bool" => type_hash_for("Bool"),
            "literal_unit" => type_hash_for("Unit"),
            "static_bytes" => {
                let data_hash = payload
                    .get("static_data")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing static_data"))?;
                let bytes_hex = self.static_data_bytes_hex(data_hash)?;
                let len = u64::try_from(bytes_hex.len() / 2)?;
                if payload.get("bytes_len").and_then(JsonValue::as_u64) != Some(len) {
                    bail!("static_bytes bytes_len mismatch");
                }
                let literal_kind = payload
                    .get("literal_kind")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing literal_kind"))?;
                match literal_kind {
                    "string" => {
                        String::from_utf8(hex_to_bytes(&bytes_hex)?)
                            .map_err(|_| anyhow!("string literal static data is not utf8"))?;
                    }
                    "bytes" => {}
                    other => bail!("unknown static literal kind {other}"),
                }
                let region = payload
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing region"))?;
                if !is_static_region(region) {
                    bail!("static_bytes region must be 'static");
                }
                let element_type = payload
                    .get("element_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing element_type"))?;
                if element_type != type_hash_for("U8") {
                    bail!("static_bytes element_type must be u8");
                }
                hash_for_type_spec(&TypeSpec::Slice {
                    region: region.to_string(),
                    mutable: false,
                    element: element_type.to_string(),
                })?
            }
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                param_types
                    .get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("param_ref out of bounds {index}"))?
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                local_type_at_depth(locals, depth)
                    .cloned()
                    .ok_or_else(|| anyhow!("local_ref out of bounds {depth}"))?
            }
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let callee = self
                    .root_symbol(root, symbol)
                    .ok_or_else(|| anyhow!("call target missing from root {symbol}"))?;
                let (expected_params, return_type) =
                    self.call_signature_with_type_args(&callee.signature, &payload)?;
                let args = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                if args.len() != expected_params.len() {
                    bail!("call arity mismatch for {symbol}");
                }
                let callee_regions = self
                    .signature_region_params(&callee.signature)?
                    .into_iter()
                    .map(|param| param.region)
                    .collect::<BTreeSet<_>>();
                let mut region_substitutions = BTreeMap::new();
                for (idx, arg) in args.iter().enumerate() {
                    let arg_hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    let arg_type = self.verify_expr_type_with_locals(
                        arg_hash,
                        root,
                        param_types,
                        allowed_regions,
                        locals,
                        fn_return,
                    )?;
                    if !self.type_assignable_for_call_in_root(
                        root,
                        &arg_type,
                        &expected_params[idx],
                        &callee_regions,
                    )? {
                        bail!("call arg type mismatch for {symbol} at arg {idx}");
                    }
                    self.infer_call_region_substitutions(
                        root,
                        &arg_type,
                        &expected_params[idx],
                        &callee_regions,
                        &mut region_substitutions,
                    )?;
                }
                self.substitute_type_regions_hash(&return_type, &region_substitutions)?
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
                let left = self.verify_expr_type_with_locals(
                    left_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let right = self.verify_expr_type_with_locals(
                    right_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let bool_hash = type_hash_for("Bool");
                match op {
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>" => {
                        if scalar_int_type_by_hash(&left).is_none() {
                            bail!("integer op requires a sized integer operand");
                        }
                        if left != right {
                            bail!("integer op operands differ");
                        }
                        left
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        if left != right {
                            bail!("comparison operands differ");
                        }
                        if scalar_int_type_by_hash(&left).is_none() {
                            bail!("comparison op requires sized integer operands");
                        }
                        bool_hash
                    }
                    "&&" | "||" => {
                        if left != bool_hash || right != bool_hash {
                            bail!("bool op requires bool operands");
                        }
                        bool_hash
                    }
                    _ => bail!("unsupported binary op {op}"),
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
                let child_type = self.verify_expr_type_with_locals(
                    child,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                match op {
                    "-" | "~" => {
                        if scalar_int_type_by_hash(&child_type).is_none() {
                            bail!("integer unary op requires a sized integer operand");
                        }
                        child_type
                    }
                    "!" => {
                        if child_type != type_hash_for("Bool") {
                            bail!("bool unary op requires bool operand");
                        }
                        type_hash_for("Bool")
                    }
                    _ => bail!("unsupported unary op {op}"),
                }
            }
            "borrow_shared" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing target"))?;
                let target_type = self.verify_expr_type_with_locals(
                    target,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let region = payload
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing region"))?;
                if !allowed_regions.contains(region) && !is_static_region(region) {
                    bail!("invalid region reference {region}");
                }
                let referent_type = payload
                    .get("referent_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing referent_type"))?;
                let expected_referent = match self.type_spec_in_root(root, &target_type)? {
                    TypeSpec::Box { element } => element,
                    _ => target_type.clone(),
                };
                if referent_type != expected_referent {
                    bail!("borrow_shared referent type mismatch");
                }
                hash_for_type_spec(&TypeSpec::Reference {
                    region: region.to_string(),
                    mutable: false,
                    referent: expected_referent,
                })?
            }
            "borrow_mut" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing target"))?;
                let target_type = self.verify_expr_type_with_locals(
                    target,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let region = payload
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing region"))?;
                if !allowed_regions.contains(region) && !is_static_region(region) {
                    bail!("invalid region reference {region}");
                }
                let referent_type = payload
                    .get("referent_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing referent_type"))?;
                let expected_referent = match self.type_spec_in_root(root, &target_type)? {
                    TypeSpec::Box { element } => element,
                    _ => target_type.clone(),
                };
                if referent_type != expected_referent {
                    bail!("borrow_mut referent type mismatch");
                }
                if !self.typed_expr_is_assignable_place(root, target)? {
                    bail!("borrow_mut target must be a mutable semantic place");
                }
                hash_for_type_spec(&TypeSpec::Reference {
                    region: region.to_string(),
                    mutable: true,
                    referent: expected_referent,
                })?
            }
            "slice_from_array" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_from_array missing target"))?;
                let target_type = self.verify_expr_type_with_locals(
                    target,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("target_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("slice_from_array target_type mismatch");
                }
                let region = payload
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_from_array missing region"))?;
                if !allowed_regions.contains(region) && !is_static_region(region) {
                    bail!("invalid region reference {region}");
                }
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("slice_from_array missing mutable"))?;
                if mutable && !self.typed_expr_is_assignable_place(root, target)? {
                    bail!("slice_from_array mutable target must be assignable");
                }
                if let TypeSpec::Reference { .. } = self.type_spec_in_root(root, &target_type)? {
                    bail!("slice_from_array target must be a fixed array, not a reference");
                }
                let IndexedElementInfo::FixedArray {
                    container_type,
                    element_type,
                    len,
                } = self.indexed_element_type_in_root(root, &target_type)?
                else {
                    bail!("slice_from_array target must be a fixed array");
                };
                if payload.get("array_type").and_then(JsonValue::as_str)
                    != Some(container_type.as_str())
                    || payload.get("element_type").and_then(JsonValue::as_str)
                        != Some(element_type.as_str())
                    || payload.get("len").and_then(JsonValue::as_u64) != Some(len)
                {
                    bail!("slice_from_array metadata mismatch");
                }
                hash_for_type_spec(&TypeSpec::Slice {
                    region: region.to_string(),
                    mutable,
                    element: element_type,
                })?
            }
            "slice_len" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_len missing target"))?;
                let target_type = self.verify_expr_type_with_locals(
                    target,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("slice_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("slice_len slice_type mismatch");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target_type)?,
                    TypeSpec::Slice { .. }
                ) {
                    bail!("slice_len target must be slice");
                }
                type_hash_for("I64")
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
                let target_type = self.verify_expr_type_with_locals(
                    target,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let TypeSpec::Slice { element, .. } = self.type_spec_in_root(root, &target_type)?
                else {
                    bail!("subslice target must be slice");
                };
                if payload.get("slice_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                    || payload.get("element_type").and_then(JsonValue::as_str)
                        != Some(element.as_str())
                {
                    bail!("subslice metadata mismatch");
                }
                if self.verify_expr_type_with_locals(
                    start,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )? != type_hash_for("I64")
                    || self.verify_expr_type_with_locals(
                        len,
                        root,
                        param_types,
                        allowed_regions,
                        locals,
                        fn_return,
                    )? != type_hash_for("I64")
                {
                    bail!("subslice start and len must be i64");
                }
                target_type
            }
            "box_new" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("box_new missing value"))?;
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("element_type").and_then(JsonValue::as_str)
                    != Some(value_type.as_str())
                {
                    bail!("box_new element_type mismatch");
                }
                hash_for_type_spec(&TypeSpec::Box {
                    element: value_type,
                })?
            }
            "unbox" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unbox missing value"))?;
                let box_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let element_type = match self.type_spec(&box_type)? {
                    TypeSpec::Box { element } => element,
                    _ => bail!("unbox requires a box value, got {box_type}"),
                };
                if payload.get("element_type").and_then(JsonValue::as_str)
                    != Some(element_type.as_str())
                {
                    bail!("unbox element_type mismatch");
                }
                if payload.get("box_type").and_then(JsonValue::as_str) != Some(box_type.as_str()) {
                    bail!("unbox box_type mismatch");
                }
                element_type
            }
            "int_cast" => {
                // Re-derive: the operand must be a sized integer, the recorded
                // `source_type` must match it, and the declared target `type` must
                // itself be a sized integer (R6).
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing value"))?;
                let source = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if scalar_int_type_by_hash(&source).is_none() {
                    bail!("int_cast operand is not a sized integer");
                }
                if payload.get("source_type").and_then(JsonValue::as_str) != Some(source.as_str()) {
                    bail!("int_cast source_type mismatch");
                }
                let target = payload
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("int_cast missing type"))?;
                if scalar_int_type_by_hash(target).is_none() {
                    bail!("int_cast target is not a sized integer");
                }
                target.to_string()
            }
            "vec_new" => {
                let capacity_hash = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_new missing capacity"))?;
                let capacity_type = self.verify_expr_type_with_locals(
                    capacity_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if capacity_type != type_hash_for("I64")
                    || payload.get("capacity_type").and_then(JsonValue::as_str)
                        != Some(capacity_type.as_str())
                {
                    bail!("vec_new capacity_type mismatch");
                }
                let capacity_value = payload
                    .get("capacity_value")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("vec_new missing capacity_value"))?;
                let Some(literal_capacity) = self.typed_literal_i64_value(capacity_hash)? else {
                    bail!("vec_new capacity must be a literal in phase 20");
                };
                if literal_capacity < 0 || capacity_value != literal_capacity as u64 {
                    bail!("vec_new capacity_value mismatch");
                }
                let element = payload
                    .get("element_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_new missing element_type"))?;
                self.validate_type_hash_in_root(root, element, allowed_regions)?;
                self.require_phase20_buffer_element(root, element, "vec_new")?;
                hash_for_type_spec(&TypeSpec::Vec {
                    element: element.to_string(),
                })?
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
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("vec_type").and_then(JsonValue::as_str) != Some(target_type.as_str())
                {
                    bail!("vec_push vec_type mismatch");
                }
                if !self.typed_expr_is_assignable_place(root, target_hash)? {
                    bail!("vec_push target must be a mutable vec place");
                }
                let TypeSpec::Vec { element } = self.type_spec_in_root(root, &target_type)? else {
                    bail!("vec_push target must be vec<T>");
                };
                if payload.get("element_type").and_then(JsonValue::as_str) != Some(element.as_str())
                {
                    bail!("vec_push element_type mismatch");
                }
                self.require_phase20_buffer_element(root, &element, "vec_push")?;
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if !self.type_assignable_in_root(root, &value_type, &element)? {
                    bail!("vec_push value type mismatch");
                }
                type_hash_for("Unit")
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
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("vec_type").and_then(JsonValue::as_str) != Some(target_type.as_str())
                {
                    bail!("vec_get vec_type mismatch");
                }
                if !self.typed_expr_is_place(target_hash)? {
                    bail!("vec_get target must be an addressable vec place");
                }
                let TypeSpec::Vec { element } = self.type_spec_in_root(root, &target_type)? else {
                    bail!("vec_get target must be vec<T>");
                };
                if payload.get("element_type").and_then(JsonValue::as_str) != Some(element.as_str())
                {
                    bail!("vec_get element_type mismatch");
                }
                self.require_phase20_buffer_element(root, &element, "vec_get")?;
                if self.verify_expr_type_with_locals(
                    index_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )? != type_hash_for("I64")
                {
                    bail!("vec_get index must be i64");
                }
                if let Some(value) = self.typed_literal_i64_value(index_hash)?
                    && value < 0
                {
                    bail!("vec_get index must be non-negative");
                }
                element
            }
            "vec_len" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("vec_len missing target"))?;
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("vec_type").and_then(JsonValue::as_str) != Some(target_type.as_str())
                {
                    bail!("vec_len vec_type mismatch");
                }
                if !self.typed_expr_is_place(target_hash)? {
                    bail!("vec_len target must be an addressable vec place");
                }
                let TypeSpec::Vec { element } = self.type_spec_in_root(root, &target_type)? else {
                    bail!("vec_len target must be vec<T>");
                };
                if payload.get("element_type").and_then(JsonValue::as_str) != Some(element.as_str())
                {
                    bail!("vec_len element_type mismatch");
                }
                self.require_phase20_buffer_element(root, &element, "vec_len")?;
                type_hash_for("I64")
            }
            "string_new" => {
                let source_hash = payload
                    .get("source")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_new missing source"))?;
                let source_type = self.verify_expr_type_with_locals(
                    source_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("source_type").and_then(JsonValue::as_str)
                    != Some(source_type.as_str())
                {
                    bail!("string_new source_type mismatch");
                }
                let TypeSpec::Slice {
                    mutable: false,
                    element,
                    ..
                } = self.type_spec_in_root(root, &source_type)?
                else {
                    bail!("string_new source must be immutable u8 slice");
                };
                if element != type_hash_for("U8") {
                    bail!("string_new source must be immutable u8 slice");
                }
                let source_payload = self.get_payload(source_hash)?;
                if source_payload.get("expr_kind").and_then(JsonValue::as_str)
                    != Some("static_bytes")
                {
                    bail!("string_new source must be static_bytes in phase 20");
                }
                let static_data = payload
                    .get("source_static_data")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_new missing source_static_data"))?;
                if source_payload
                    .get("static_data")
                    .and_then(JsonValue::as_str)
                    != Some(static_data)
                {
                    bail!("string_new source_static_data mismatch");
                }
                let bytes_hex = self.static_data_bytes_hex(static_data)?;
                if payload.get("bytes_len").and_then(JsonValue::as_u64)
                    != Some((bytes_hex.len() / 2) as u64)
                {
                    bail!("string_new bytes_len mismatch");
                }
                hash_for_type_spec(&TypeSpec::String)?
            }
            "string_len" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_len missing target"))?;
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("string_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("string_len string_type mismatch");
                }
                if !self.typed_expr_is_place(target_hash)? {
                    bail!("string_len target must be an addressable string place");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target_type)?,
                    TypeSpec::String
                ) {
                    bail!("string_len target must be string");
                }
                type_hash_for("I64")
            }
            "arg_count" => type_hash_for("I64"),
            kind @ ("arg_len" | "arg_byte") => {
                // Process-argument reads (R12): every operand is an i64 index.
                let keys: &[&str] = if kind == "arg_len" {
                    &["index"]
                } else {
                    &["index", "byte"]
                };
                for key in keys {
                    let child = payload
                        .get(*key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("{kind} missing {key}"))?;
                    let child_type = self.verify_expr_type_with_locals(
                        child,
                        root,
                        param_types,
                        allowed_regions,
                        locals,
                        fn_return,
                    )?;
                    if child_type != type_hash_for("I64") {
                        bail!("{kind} {key} must be i64");
                    }
                }
                if kind == "arg_len" {
                    type_hash_for("I64")
                } else {
                    type_hash_for("U8")
                }
            }
            "string_with_capacity" => {
                let capacity_hash = payload
                    .get("capacity")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_with_capacity missing capacity"))?;
                let capacity_type = self.verify_expr_type_with_locals(
                    capacity_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if capacity_type != type_hash_for("I64")
                    || payload.get("capacity_type").and_then(JsonValue::as_str)
                        != Some(capacity_type.as_str())
                {
                    bail!("string_with_capacity capacity_type mismatch");
                }
                hash_for_type_spec(&TypeSpec::String)?
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
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("string_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("string_push string_type mismatch");
                }
                if !self.typed_expr_is_assignable_place(root, target_hash)? {
                    bail!("string_push target must be a mutable string place");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target_type)?,
                    TypeSpec::String
                ) {
                    bail!("string_push target must be string");
                }
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if value_type != type_hash_for("U8") {
                    bail!("string_push value must be u8");
                }
                type_hash_for("Unit")
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
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("string_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("string_get string_type mismatch");
                }
                if !self.typed_expr_is_place(target_hash)? {
                    bail!("string_get target must be an addressable string place");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target_type)?,
                    TypeSpec::String
                ) {
                    bail!("string_get target must be string");
                }
                let index_type = self.verify_expr_type_with_locals(
                    index_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if index_type != type_hash_for("I64") {
                    bail!("string_get index must be i64");
                }
                type_hash_for("U8")
            }
            "string_set" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_set missing target"))?;
                let index_hash = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_set missing index"))?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("string_set missing value"))?;
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("string_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("string_set string_type mismatch");
                }
                if !self.typed_expr_is_assignable_place(root, target_hash)? {
                    bail!("string_set target must be a mutable string place");
                }
                if !matches!(
                    self.type_spec_in_root(root, &target_type)?,
                    TypeSpec::String
                ) {
                    bail!("string_set target must be string");
                }
                let index_type = self.verify_expr_type_with_locals(
                    index_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if index_type != type_hash_for("I64") {
                    bail!("string_set index must be i64");
                }
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if value_type != type_hash_for("U8") {
                    bail!("string_set value must be u8");
                }
                type_hash_for("Unit")
            }
            "raw_ptr_cast" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing value"))?;
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("source_type").and_then(JsonValue::as_str)
                    != Some(value_type.as_str())
                {
                    bail!("raw_ptr_cast source_type mismatch");
                }
                let mutable = payload
                    .get("mutable")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing mutable"))?;
                let pointee_type = payload
                    .get("pointee_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_ptr_cast missing pointee_type"))?;
                match self.type_spec_in_root(root, &value_type)? {
                    TypeSpec::Reference {
                        mutable: source_mutable,
                        referent,
                        ..
                    } => {
                        if referent != pointee_type {
                            bail!("raw_ptr_cast pointee_type mismatch");
                        }
                        if mutable && !source_mutable {
                            bail!(
                                "raw_ptr_cast cannot create raw mutable pointer from shared reference"
                            );
                        }
                    }
                    TypeSpec::RawPointer {
                        mutable: source_mutable,
                        pointee,
                    } => {
                        if pointee != pointee_type {
                            bail!("raw_ptr_cast pointee_type mismatch");
                        }
                        if mutable && !source_mutable {
                            bail!("raw_ptr_cast cannot cast raw shared pointer to mutable");
                        }
                    }
                    _ => bail!("raw_ptr_cast source must be reference or raw pointer"),
                }
                hash_for_type_spec(&TypeSpec::RawPointer {
                    mutable,
                    pointee: pointee_type.to_string(),
                })?
            }
            "raw_load" => {
                let pointer_hash = payload
                    .get("pointer")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_load missing pointer"))?;
                let pointer_type = self.verify_expr_type_with_locals(
                    pointer_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("pointer_type").and_then(JsonValue::as_str)
                    != Some(pointer_type.as_str())
                {
                    bail!("raw_load pointer_type mismatch");
                }
                let TypeSpec::RawPointer { pointee, .. } =
                    self.type_spec_in_root(root, &pointer_type)?
                else {
                    bail!("raw_load pointer must be raw pointer");
                };
                if payload.get("pointee_type").and_then(JsonValue::as_str) != Some(pointee.as_str())
                {
                    bail!("raw_load pointee_type mismatch");
                }
                let class = self.value_class_in_root(root, &pointee)?;
                if class.copy_kind != ValueCopyKind::Copy
                    || class.drop_kind != ValueDropKind::Trivial
                    || class.contains_reference
                {
                    bail!(
                        "raw_load currently supports only Copy, non-reference values with trivial drop"
                    );
                }
                pointee
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
                let pointer_type = self.verify_expr_type_with_locals(
                    pointer_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("pointer_type").and_then(JsonValue::as_str)
                    != Some(pointer_type.as_str())
                {
                    bail!("raw_store pointer_type mismatch");
                }
                let TypeSpec::RawPointer {
                    mutable: true,
                    pointee,
                } = self.type_spec_in_root(root, &pointer_type)?
                else {
                    bail!("raw_store pointer must be raw mutable pointer");
                };
                if payload.get("pointee_type").and_then(JsonValue::as_str) != Some(pointee.as_str())
                {
                    bail!("raw_store pointee_type mismatch");
                }
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if !self.type_assignable_in_root(root, &value_type, &pointee)? {
                    bail!("raw_store value type mismatch");
                }
                let class = self.value_class_in_root(root, &pointee)?;
                if class.copy_kind != ValueCopyKind::Copy
                    || class.drop_kind != ValueDropKind::Trivial
                    || class.contains_reference
                {
                    bail!(
                        "raw_store currently supports only Copy, non-reference values with trivial drop"
                    );
                }
                type_hash_for("Unit")
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
                let target_type = self.verify_expr_type_with_locals(
                    target,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let value_type = self.verify_expr_type_with_locals(
                    value,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("target_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("assign target_type mismatch");
                }
                if !self.typed_expr_is_assignable_place(root, target)? {
                    bail!("assign target must be a mutable semantic place");
                }
                if !self.type_assignable_in_root(root, &value_type, &target_type)? {
                    bail!("assign value type mismatch");
                }
                type_hash_for("Unit")
            }
            "let" => {
                let binding_type = payload
                    .get("binding_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_type"))?
                    .to_string();
                let binding_name = payload
                    .get("binding_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_name"))?;
                validate_projection_identifier("let binding", binding_name)?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing value"))?;
                let body_hash = payload
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing body"))?;
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if !self.type_assignable_in_root(root, &value_type, &binding_type)? {
                    bail!("let binding type mismatch");
                }
                locals.push(binding_type);
                let body_type = self.verify_expr_type_with_locals(
                    body_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                );
                locals.pop();
                body_type?
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
                let cond_type = self.verify_expr_type_with_locals(
                    cond,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if cond_type != type_hash_for("Bool") {
                    bail!("if condition must be bool");
                }
                let then_div = self.typed_expr_diverges(then_hash)?;
                let else_div = self.typed_expr_diverges(else_hash)?;
                let then_type = self.verify_expr_type_with_locals(
                    then_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let else_type = self.verify_expr_type_with_locals(
                    else_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                // Early exit (R7): a divergent branch fixes no type — the
                // non-divergent branch does (mirrors the type-checker join).
                if then_div && !else_div {
                    else_type
                } else if else_div && !then_div {
                    then_type
                } else {
                    if !then_div && then_type != else_type {
                        bail!("if branches must have the same type");
                    }
                    then_type
                }
            }
            "return" => {
                // `return <value>` (R7): re-derive the operand's type; the node's
                // declared type is the operand's, so this matches by construction.
                // The operand is what the function actually delivers, so it must
                // be assignable to the enclosing function's declared return type
                // regardless of the position the `return` node sits in (#13).
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("return missing value"))?;
                let operand_type = self.verify_expr_type_with_locals(
                    value,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let Some(return_type) = fn_return else {
                    bail!("bad_type: return used outside a function body context");
                };
                if !self.type_assignable_in_root(root, &operand_type, return_type)? {
                    bail!(
                        "bad_type: return operand is {}, function returns {}",
                        self.type_name(&operand_type)?,
                        self.type_name(return_type)?
                    );
                }
                operand_type
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
                validate_projection_identifier("fold item binding", item_name)?;
                validate_projection_identifier("fold accumulator binding", acc_name)?;
                if item_name == acc_name {
                    bail!("fold item and accumulator bindings must be distinct");
                }
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
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if payload.get("target_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("fold target_type mismatch");
                }
                let info = self.indexed_element_type_in_root(root, &target_type)?;
                let (expected_kind, element_type, fixed_len) = match info {
                    IndexedElementInfo::FixedArray {
                        element_type, len, ..
                    } => {
                        if !self.typed_expr_is_place(target_hash)? {
                            bail!("fold over a fixed array requires an addressable array place");
                        }
                        ("fixed_array", element_type, Some(len))
                    }
                    IndexedElementInfo::Slice { element_type, .. } => ("slice", element_type, None),
                };
                if payload.get("target_kind").and_then(JsonValue::as_str) != Some(expected_kind)
                    || payload.get("element_type").and_then(JsonValue::as_str)
                        != Some(element_type.as_str())
                {
                    bail!("fold target metadata mismatch");
                }
                if let Some(len) = fixed_len
                    && payload.get("len").and_then(JsonValue::as_u64) != Some(len)
                {
                    bail!("fold fixed array length metadata mismatch");
                }
                let element_class = self.value_class_in_root(root, &element_type)?;
                if element_class.copy_kind == ValueCopyKind::MoveOnly {
                    bail!("fold element type must be copyable in phase 13");
                }
                if element_class.contains_reference {
                    bail!("fold element type must not carry references in phase 13");
                }
                let init_type = self.verify_expr_type_with_locals(
                    init_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                // `acc_type` may be anchored to an enclosing binding's named type
                // (see `type_fold_expr`), so require the init to be assignable to
                // it rather than identical. `acc_type` then governs the accumulator
                // slot, the body type, and the fold result type.
                let acc_type = payload
                    .get("acc_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing acc_type"))?
                    .to_string();
                if !self.type_assignable_in_root(root, &init_type, &acc_type)? {
                    bail!("fold acc_type mismatch");
                }
                let acc_class = self.value_class_in_root(root, &acc_type)?;
                if acc_class.copy_kind == ValueCopyKind::MoveOnly {
                    bail!("fold accumulator type must be copyable in phase 13");
                }
                if acc_class.contains_reference {
                    bail!("fold accumulator type must not carry references in phase 13");
                }
                locals.push(element_type);
                locals.push(acc_type.clone());
                let body_type = self.verify_expr_type_with_locals(
                    body_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                );
                locals.pop();
                locals.pop();
                let body_type = body_type?;
                if !self.type_assignable_in_root(root, &body_type, &acc_type)? {
                    bail!("fold body type mismatch");
                }
                acc_type
            }
            "loop" => {
                let acc_name = payload
                    .get("acc_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing acc_name"))?;
                validate_projection_identifier("loop accumulator binding", acc_name)?;
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
                let init_type = self.verify_expr_type_with_locals(
                    init_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let acc_type = payload
                    .get("acc_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("loop missing acc_type"))?
                    .to_string();
                if !self.type_assignable_in_root(root, &init_type, &acc_type)? {
                    bail!("loop acc_type mismatch");
                }
                // The accumulator may be move-only (loop-carried drop glue,
                // SPEC_V3 §7) but must not carry references — a loan cannot
                // outlive the iteration that minted it.
                let acc_class = self.value_class_in_root(root, &acc_type)?;
                if acc_class.contains_reference {
                    bail!("loop accumulator type must not carry references");
                }
                locals.push(acc_type.clone());
                let cond_type = self.verify_expr_type_with_locals(
                    cond_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                );
                let body_type = self.verify_expr_type_with_locals(
                    body_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                );
                locals.pop();
                let cond_type = cond_type?;
                let body_type = body_type?;
                if cond_type != type_hash_for("Bool") {
                    bail!("loop condition must be bool");
                }
                if !self.type_assignable_in_root(root, &body_type, &acc_type)? {
                    bail!("loop body type mismatch");
                }
                acc_type
            }
            "array_literal" => {
                let elements = payload
                    .get("elements")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("array_literal missing elements"))?;
                if elements.is_empty() {
                    bail!("array literal must have at least one element");
                }
                let element_type = payload
                    .get("element_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_literal missing element_type"))?
                    .to_string();
                let len = payload
                    .get("len")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("array_literal missing len"))?;
                if len != elements.len() as u64 {
                    bail!("array_literal len mismatch");
                }
                for (idx, element) in elements.iter().enumerate() {
                    let value_hash = element
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("array element missing value"))?;
                    let value_type = self.verify_expr_type_with_locals(
                        value_hash,
                        root,
                        param_types,
                        allowed_regions,
                        locals,
                        fn_return,
                    )?;
                    if element.get("type").and_then(JsonValue::as_str) != Some(value_type.as_str())
                    {
                        bail!("array element type metadata mismatch at index {idx}");
                    }
                    if !self.type_assignable_in_root(root, &value_type, &element_type)? {
                        bail!("array element type mismatch at index {idx}");
                    }
                }
                hash_for_type_spec(&TypeSpec::FixedArray {
                    element: element_type,
                    len,
                })?
            }
            "array_fill" => {
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing value"))?;
                let element_type = payload
                    .get("element_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_fill missing element_type"))?
                    .to_string();
                let count = payload
                    .get("count")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("array_fill missing count"))?;
                if count == 0 {
                    bail!("array fill count must be at least 1");
                }
                if count > MAX_FIXED_ARRAY_LEN {
                    bail!(
                        "array fill count {count} exceeds the supported maximum {MAX_FIXED_ARRAY_LEN}"
                    );
                }
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if value_type != element_type {
                    bail!("array_fill element_type mismatch");
                }
                let class = self.value_class_in_root(root, &element_type)?;
                if class.copy_kind != ValueCopyKind::Copy
                    || class.drop_kind != ValueDropKind::Trivial
                    || class.contains_reference
                {
                    bail!("array fill value must be a non-reference Copy value with trivial drop");
                }
                hash_for_type_spec(&TypeSpec::FixedArray {
                    element: element_type,
                    len: count,
                })?
            }
            "array_set" => {
                // Recompute: `array` is `array<T, N>`, `index` is i64, `value` is T
                // (a non-reference Copy element with trivial drop); the result is the
                // array type unchanged.
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
                let array_type = self.verify_expr_type_with_locals(
                    array_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let TypeSpec::FixedArray { element, len } =
                    self.type_spec_in_root(root, &array_type)?
                else {
                    bail!("array_set target must be a fixed array");
                };
                if payload.get("element_type").and_then(JsonValue::as_str) != Some(element.as_str())
                {
                    bail!("array_set element_type mismatch");
                }
                let class = self.value_class_in_root(root, &element)?;
                if class.copy_kind != ValueCopyKind::Copy
                    || class.drop_kind != ValueDropKind::Trivial
                    || class.contains_reference
                {
                    bail!(
                        "array_set element must be a non-reference Copy value with trivial drop"
                    );
                }
                let index_type = self.verify_expr_type_with_locals(
                    index_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if index_type != type_hash_for("I64") {
                    bail!("array_set index must be i64");
                }
                if let Some(value) = self.typed_literal_i64_value(index_hash)?
                    && (value < 0 || value as u64 >= len)
                {
                    bail!("array_set index {value} out of bounds for length {len}");
                }
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if !self.type_assignable_in_root(root, &value_type, &element)? {
                    bail!("array_set value type does not match element type");
                }
                array_type
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
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let index_type = self.verify_expr_type_with_locals(
                    index_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if index_type != type_hash_for("I64") {
                    bail!("array index must be i64");
                }
                if payload.get("target_type").and_then(JsonValue::as_str)
                    != Some(target_type.as_str())
                {
                    bail!("array_index target_type mismatch");
                }
                match self.indexed_element_type_in_root(root, &target_type)? {
                    IndexedElementInfo::FixedArray {
                        container_type,
                        element_type,
                        len,
                    } => {
                        if payload.get("indexed_kind").and_then(JsonValue::as_str)
                            != Some("fixed_array")
                            || payload.get("array_type").and_then(JsonValue::as_str)
                                != Some(container_type.as_str())
                            || payload.get("element_type").and_then(JsonValue::as_str)
                                != Some(element_type.as_str())
                            || payload.get("len").and_then(JsonValue::as_u64) != Some(len)
                        {
                            bail!("array_index metadata mismatch");
                        }
                        if let Some(value) = self.typed_literal_i64_value(index_hash)?
                            && (value < 0 || value as u64 >= len)
                        {
                            bail!("array index {value} out of bounds for length {len}");
                        }
                        element_type
                    }
                    IndexedElementInfo::Slice {
                        container_type,
                        element_type,
                    } => {
                        if payload.get("indexed_kind").and_then(JsonValue::as_str) != Some("slice")
                            || payload.get("slice_type").and_then(JsonValue::as_str)
                                != Some(container_type.as_str())
                            || payload.get("element_type").and_then(JsonValue::as_str)
                                != Some(element_type.as_str())
                        {
                            bail!("slice index metadata mismatch");
                        }
                        if let Some(value) = self.typed_literal_i64_value(index_hash)?
                            && value < 0
                        {
                            bail!("slice index must be non-negative, got {value}");
                        }
                        element_type
                    }
                }
            }
            "record_literal" => {
                let mut names = BTreeSet::new();
                let mut fields = Vec::new();
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
                    validate_projection_identifier("record field", &name)?;
                    if !names.insert(name.clone()) {
                        bail!("duplicate record field {name}");
                    }
                    let value_hash = field
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("record field missing value"))?;
                    let field_type = self.verify_expr_type_with_locals(
                        value_hash,
                        root,
                        param_types,
                        allowed_regions,
                        locals,
                        fn_return,
                    )?;
                    if field.get("type").and_then(JsonValue::as_str) != Some(field_type.as_str()) {
                        bail!("record field type mismatch for {name}");
                    }
                    fields.push(TypeFieldSpec {
                        name,
                        type_hash: field_type,
                    });
                }
                hash_for_type_spec(&TypeSpec::Record(fields))?
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
                let target_type = self.verify_expr_type_with_locals(
                    target_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                self.field_access_type_in_root(root, &target_type, field)?
            }
            "enum_construct" => {
                let enum_type = payload
                    .get("enum_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing enum_type"))?;
                if declared_type != enum_type {
                    bail!("enum_construct declared type must match enum_type");
                }
                let variant = payload
                    .get("variant")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing variant"))?;
                validate_projection_identifier("enum variant", variant)?;
                let variant_type = self.enum_variant_type_in_root(root, enum_type, variant)?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                let value_type = self.verify_expr_type_with_locals(
                    value_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                if value_type != variant_type
                    && !self.type_assignable_in_root(root, &value_type, &variant_type)?
                {
                    bail!("enum variant payload type mismatch for {variant}");
                }
                enum_type.to_string()
            }
            "case" => {
                let scrutinee_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                let scrutinee_type = self.verify_expr_type_with_locals(
                    scrutinee_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                    fn_return,
                )?;
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?;
                // Scalar literal `case` (R14): re-verify the literal-pattern arms.
                if scrutinee_type == type_hash_for("I64")
                    || scrutinee_type == type_hash_for("Bool")
                {
                    return self.verify_scalar_case_type(
                        &scrutinee_type,
                        arms,
                        &declared_type,
                        root,
                        param_types,
                        allowed_regions,
                        locals,
                        fn_return,
                    );
                }
                let TypeSpec::Enum(variants) = self.type_spec_in_root(root, &scrutinee_type)?
                else {
                    bail!("case scrutinee must be an enum or scalar (i64/bool)");
                };
                let mut seen = BTreeSet::new();
                let mut variant_kind: BTreeMap<String, bool> = BTreeMap::new();
                let mut has_nested = false;
                let mut arm_patterns: Vec<RawPattern> = Vec::new();
                let mut result_type = None;
                // First arm's body type, used only when every arm diverges (R7).
                let mut fallback_type: Option<String> = None;
                let mut has_default = false;
                for (index, arm) in arms.iter().enumerate() {
                    if arm.get("guard").is_some() {
                        bail!("if-guards are only supported on i64 scalar `case` arms");
                    }
                    let is_default = arm.get("default").and_then(JsonValue::as_bool) == Some(true);
                    // The type of the (at most one) leaf binding the arm body scopes.
                    let scoped_type: Option<String>;
                    if is_default {
                        if index + 1 != arms.len() {
                            bail!("default case arm must be last");
                        }
                        if has_default {
                            bail!("duplicate default case arm");
                        }
                        if arm.get("variant").is_some() {
                            bail!("default case arm cannot specify a variant");
                        }
                        if arm.get("binding_name").is_some() {
                            bail!("default case arm cannot bind a payload");
                        }
                        if arm.get("payload_pattern").is_some() {
                            bail!("default case arm cannot carry a pattern");
                        }
                        has_default = true;
                        scoped_type = None;
                    } else {
                        let variant = arm
                            .get("variant")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing variant"))?;
                        validate_projection_identifier("enum variant", variant)?;
                        let variant_type = variants
                            .iter()
                            .find(|candidate| candidate.name == variant)
                            .map(|candidate| candidate.type_hash.clone())
                            .ok_or_else(|| anyhow!("case arm uses unknown variant {variant}"))?;
                        if let Some(node) = arm.get("payload_pattern") {
                            // Nested destructuring arm (R14): re-verify the chain and
                            // scope its leaf. Multiple share an outer variant; a simple
                            // arm for the same variant is rejected as a mix.
                            has_nested = true;
                            match variant_kind.get(variant) {
                                Some(true) => {}
                                Some(false) => bail!(
                                    "case arm {variant} cannot mix a binding pattern with nested destructuring patterns"
                                ),
                                None => {
                                    variant_kind.insert(variant.to_string(), true);
                                }
                            }
                            scoped_type = self
                                .verify_payload_pattern(root, node, &variant_type)?
                                .map(|(_, type_hash)| type_hash);
                        } else {
                            match variant_kind.get(variant) {
                                Some(true) => bail!(
                                    "case arm {variant} cannot mix a binding pattern with nested destructuring patterns"
                                ),
                                Some(false) => bail!("duplicate case arm {variant}"),
                                None => {
                                    variant_kind.insert(variant.to_string(), false);
                                }
                            }
                            if let Some(binding) = arm.get("binding_name").and_then(JsonValue::as_str)
                            {
                                validate_projection_identifier("case binding", binding)?;
                                scoped_type = Some(variant_type.clone());
                            } else if variant_type != type_hash_for("Unit") {
                                bail!("case arm {variant} must bind its payload");
                            } else {
                                scoped_type = None;
                            }
                        }
                        seen.insert(variant.to_string());
                        if let Some(pattern) = crate::expr::typed_arm_scrutinee_pattern(arm) {
                            arm_patterns.push(pattern);
                        }
                    }
                    let body_hash = arm
                        .get("body")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("case arm missing body"))?;
                    if let Some(type_hash) = &scoped_type {
                        locals.push(type_hash.clone());
                    }
                    let body_type = self.verify_expr_type_with_locals(
                        body_hash,
                        root,
                        param_types,
                        allowed_regions,
                        locals,
                        fn_return,
                    );
                    if scoped_type.is_some() {
                        locals.pop();
                    }
                    let body_type = body_type?;
                    if fallback_type.is_none() {
                        fallback_type = Some(body_type.clone());
                    }
                    // Early exit (R7): a divergent arm neither fixes nor must match
                    // the result type (mirrors the type-checker join).
                    if !self.typed_expr_diverges(body_hash)? {
                        if let Some(expected) = &result_type {
                            if expected != &body_type {
                                bail!("case arm type mismatch");
                            }
                        } else {
                            result_type = Some(body_type);
                        }
                    }
                }
                let expected_variants = variants
                    .iter()
                    .map(|variant| variant.name.clone())
                    .collect::<BTreeSet<_>>();
                if has_nested {
                    self.check_patterns_well_formed(root, &scrutinee_type, &arm_patterns)?;
                }
                if !has_default {
                    if has_nested {
                        if !self.patterns_exhaustive(root, &scrutinee_type, &arm_patterns)? {
                            bail!(
                                "case expression is not exhaustive: every variant and nested sub-pattern must be covered, or add a `_` arm"
                            );
                        }
                    } else if seen != expected_variants {
                        bail!("case expression must cover every enum variant");
                    }
                }
                result_type
                    .or(fallback_type)
                    .ok_or_else(|| anyhow!("case expression has no arms"))?
            }
            other => bail!("unknown expression kind {other}"),
        };
        if declared_type != actual_type {
            bail!(
                "bad_type: expression {expr_hash} declares {declared_type}, actual {actual_type}"
            );
        }
        Ok(actual_type)
    }
}

fn require_type(actual: &str, expected: &str, label: &str, db: &CodeDb) -> Result<()> {
    if actual != expected {
        bail!(
            "{label} expected {}, got {}",
            db.type_name(expected)?,
            db.type_name(actual)?
        );
    }
    Ok(())
}

fn local_binding_at_name<'a>(
    locals: &'a [LocalTypeBinding],
    name: &str,
) -> Option<(usize, &'a LocalTypeBinding)> {
    locals
        .iter()
        .enumerate()
        .rev()
        .find(|(_, binding)| binding.name == name)
        .map(|(idx, binding)| (locals.len() - 1 - idx, binding))
}

fn local_type_at_depth(locals: &[String], depth: usize) -> Option<&String> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
}

fn local_bool_at_depth(locals: &[bool], depth: usize) -> Option<bool> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
        .copied()
}

fn local_usize_at_depth(locals: &[usize], depth: usize) -> Option<usize> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
        .copied()
}

fn synthetic_let_local_id(locals: &[usize]) -> usize {
    usize::MAX - locals.len()
}

fn loan_owner_overlaps(loan: &ActiveLoan, owner: &LoanPlace) -> bool {
    loan.owner
        .as_ref()
        .is_some_and(|loan_owner| places_overlap(loan_owner, owner))
}

fn rebased_loan_owner(
    existing_owner: Option<&LoanPlace>,
    source_owner: Option<&LoanPlace>,
    target_owner: &LoanPlace,
) -> LoanPlace {
    let Some(existing_owner) = existing_owner else {
        return target_owner.clone();
    };
    let Some(source_owner) = source_owner else {
        return target_owner.clone();
    };
    if existing_owner.root != source_owner.root
        || !fields_prefix(&source_owner.fields, &existing_owner.fields)
    {
        return target_owner.clone();
    }
    let mut rebased = target_owner.clone();
    rebased.fields.extend(
        existing_owner.fields[source_owner.fields.len()..]
            .iter()
            .cloned(),
    );
    rebased
}

fn alternative_value_loans(left: Vec<ActiveLoan>, right: Vec<ActiveLoan>) -> Vec<ActiveLoan> {
    let mut out = Vec::new();
    for loan in left.iter().chain(right.iter()) {
        let needed = loan_count(&left, loan).max(loan_count(&right, loan));
        let existing = loan_count(&out, loan);
        if existing < needed {
            out.push(loan.clone());
        }
    }
    out
}

fn loan_count(loans: &[ActiveLoan], needle: &ActiveLoan) -> usize {
    loans.iter().filter(|loan| *loan == needle).count()
}

fn merge_branch_state(
    state: &mut MoveBorrowState,
    then_state: MoveBorrowState,
    else_state: MoveBorrowState,
) {
    *state = merged_branch_states(then_state, else_state);
}

fn merged_branch_states(mut left: MoveBorrowState, right: MoveBorrowState) -> MoveBorrowState {
    left.next_local = left.next_local.max(right.next_local);
    for loan in right.active {
        if !left.active.contains(&loan) {
            left.active.push(loan);
        }
    }
    for moved in right.moved {
        if !left.moved.contains(&moved) {
            left.moved.push(moved);
        }
    }
    left
}

fn places_overlap(left: &LoanPlace, right: &LoanPlace) -> bool {
    if left.root != right.root {
        return false;
    }
    fields_prefix(&left.fields, &right.fields) || fields_prefix(&right.fields, &left.fields)
}

/// True when `prefix` denotes `value` or an ancestor place of it (same root and
/// a leading field-path prefix), with NO `[*]` wildcard matching: every path
/// segment must be literally equal. Unlike [`places_overlap`] this is
/// directional, and unlike a wildcard prefix it must *prove* that one place
/// denotes (or is an ancestor of) another — for example when deciding that an
/// assignment definitely overwrites the storage a loan refers to. A dynamic
/// `[*]` index denotes an unknown element and therefore proves nothing, so it
/// must never satisfy a definite-overwrite test (see the assignment loan
/// retirement in `verify_expr_borrows`); the wildcard `[*]` ambiguity is handled
/// separately there via [`places_overlap`].
fn place_is_prefix_of_exact(prefix: &LoanPlace, value: &LoanPlace) -> bool {
    prefix.root == value.root && fields_prefix_exact(&prefix.fields, &value.fields)
}

fn fields_prefix(prefix: &[String], value: &[String]) -> bool {
    prefix.len() <= value.len()
        && prefix
            .iter()
            .zip(value.iter())
            .all(|(left, right)| left == right || left == "[*]" || right == "[*]")
}

/// Exact (wildcard-free) variant of [`fields_prefix`]: a `[*]` segment matches
/// only another literal `[*]`, never a concrete `[N]`.
fn fields_prefix_exact(prefix: &[String], value: &[String]) -> bool {
    prefix.len() <= value.len()
        && prefix
            .iter()
            .zip(value.iter())
            .all(|(left, right)| left == right)
}

fn array_index_segment(index: u64) -> String {
    format!("[{index}]")
}

fn named_actual_type_assignable(actual: &TypeSpec, expected: &TypeSpec) -> Option<bool> {
    let TypeSpec::Named {
        type_symbol: actual_symbol,
        region_args: actual_args,
        type_args: actual_type_args,
    } = actual
    else {
        return None;
    };
    let TypeSpec::Named {
        type_symbol: expected_symbol,
        region_args: expected_args,
        type_args: expected_type_args,
    } = expected
    else {
        return Some(false);
    };
    Some(
        actual_symbol == expected_symbol
            && actual_args == expected_args
            && actual_type_args == expected_type_args,
    )
}

fn named_actual_type_assignable_for_call(
    actual: &TypeSpec,
    expected: &TypeSpec,
    callee_regions: &BTreeSet<String>,
) -> Option<bool> {
    let TypeSpec::Named {
        type_symbol: actual_symbol,
        region_args: actual_args,
        type_args: actual_type_args,
    } = actual
    else {
        return None;
    };
    let TypeSpec::Named {
        type_symbol: expected_symbol,
        region_args: expected_args,
        type_args: expected_type_args,
    } = expected
    else {
        return Some(false);
    };
    if actual_symbol != expected_symbol
        || actual_args.len() != expected_args.len()
        || actual_type_args != expected_type_args
    {
        return Some(false);
    }
    Some(
        actual_args
            .iter()
            .zip(expected_args)
            .all(|(actual_region, expected_region)| {
                actual_region == expected_region || callee_regions.contains(expected_region)
            }),
    )
}

fn record_call_region_substitution(
    expected_region: String,
    actual_region: String,
    callee_regions: &BTreeSet<String>,
    substitutions: &mut BTreeMap<String, String>,
) -> Result<()> {
    if !callee_regions.contains(&expected_region) {
        return Ok(());
    }
    match substitutions.get(&expected_region) {
        Some(existing) if existing != &actual_region => bail!(
            "call region inference conflict for {expected_region}: {existing} vs {actual_region}"
        ),
        Some(_) => Ok(()),
        None => {
            substitutions.insert(expected_region, actual_region);
            Ok(())
        }
    }
}

/// An enum `case` arm as a pattern over the *scrutinee* enum (R14): the outer
/// `variant` wrapping its payload matcher — a nested `payload_pattern`, a simple
/// `binding` (a catch-all on the payload), or a no-binding wildcard. Drives the
/// recursive nested-exhaustiveness check.
fn arm_scrutinee_pattern(arm: &RawCaseArm, variant: &str) -> RawPattern {
    let sub = if let Some(pattern) = &arm.payload_pattern {
        pattern.clone()
    } else if let Some(binding) = &arm.binding {
        RawPattern::Binding(binding.clone())
    } else {
        RawPattern::Wildcard
    };
    RawPattern::Variant {
        variant: variant.to_string(),
        sub: Box::new(sub),
    }
}

pub(crate) fn type_hash_for(type_kind: &str) -> String {
    hash_object_canonical(
        "Type",
        SCHEMA_VERSION,
        &canonical_json(&json!({ "type_kind": type_kind })),
    )
}

pub(crate) fn normalize_effects(effects: &[Effect]) -> Result<Vec<Effect>> {
    let mut set = effects.iter().copied().collect::<BTreeSet<_>>();
    if set.contains(&Effect::Pure) && set.len() > 1 {
        bail!("pure effect cannot be combined with other effects");
    }
    if set.remove(&Effect::Pure) {
        return Ok(Vec::new());
    }
    Ok(set.into_iter().collect())
}

pub(crate) fn visible_effects(effects: &[Effect]) -> Vec<Effect> {
    if effects.is_empty() {
        vec![Effect::Pure]
    } else {
        effects.to_vec()
    }
}

pub(crate) fn effect_names(effects: &[Effect]) -> Vec<&'static str> {
    effects.iter().map(|effect| effect.as_str()).collect()
}

pub(crate) fn validate_external_abi_tag(abi: &str) -> Result<()> {
    match abi {
        "c" => Ok(()),
        other => bail!("unsupported external ABI tag {other}; supported ABI tags: c"),
    }
}

pub(crate) fn validate_external_link_name(name: &str) -> Result<()> {
    if !is_native_link_identifier(name) {
        bail!("external link_name must be a native identifier: {name:?}");
    }
    Ok(())
}

pub(crate) fn validate_external_library_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.')
        })
    {
        bail!("external library name must be non-empty and contain only alnum, _, -, or .");
    }
    Ok(())
}

fn is_native_link_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '_' && !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

impl TypeSpec {
    pub(crate) fn to_source(&self, db: &CodeDb) -> Result<String> {
        match self {
            TypeSpec::Builtin(kind) => match kind.as_str() {
                "Bool" => Ok("bool".to_string()),
                "Unit" => Ok("unit".to_string()),
                other => scalar_int_source_name(other)
                    .ok_or_else(|| anyhow!("unknown builtin type kind {other}")),
            },
            TypeSpec::TypeParam { index } => Ok(format!("typeparam<{index}>")),
            TypeSpec::Named {
                type_symbol,
                region_args,
                type_args,
            } => {
                let args = region_args
                    .iter()
                    .cloned()
                    .chain(type_args.iter().map(|arg| format!("type<{arg}>")))
                    .collect::<Vec<_>>();
                if args.is_empty() {
                    Ok(format!("type<{type_symbol}>"))
                } else {
                    Ok(format!("type<{type_symbol}<{}>>", args.join(", ")))
                }
            }
            TypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let referent = db.type_name(referent)?;
                if *mutable {
                    Ok(format!("&'{region} mut {referent}"))
                } else {
                    Ok(format!("&'{region} {referent}"))
                }
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                let pointee = db.type_name(pointee)?;
                if *mutable {
                    Ok(format!("raw_mut_ptr<{pointee}>"))
                } else {
                    Ok(format!("raw_ptr<{pointee}>"))
                }
            }
            TypeSpec::Box { element } => Ok(format!("box<{}>", db.type_name(element)?)),
            TypeSpec::Vec { element } => Ok(format!("vec<{}>", db.type_name(element)?)),
            TypeSpec::String => Ok("string".to_string()),
            TypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let element = db.type_name(element)?;
                if *mutable {
                    Ok(format!("mut_slice<'{region}, {element}>"))
                } else {
                    Ok(format!("slice<'{region}, {element}>"))
                }
            }
            TypeSpec::FixedArray { element, len } => {
                Ok(format!("array<{}, {len}>", db.type_name(element)?))
            }
            TypeSpec::Record(fields) => {
                let rendered = fields
                    .iter()
                    .map(|field| {
                        Ok(format!(
                            "{}: {}",
                            field.name,
                            db.type_name(&field.type_hash)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("record {{{}}}", rendered.join(", ")))
            }
            TypeSpec::Enum(variants) => {
                let rendered = variants
                    .iter()
                    .map(|variant| {
                        Ok(format!(
                            "{}: {}",
                            variant.name,
                            db.type_name(&variant.type_hash)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("enum {{{}}}", rendered.join(", ")))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTypeField {
    name: String,
    ty: ParsedTypeSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedTypeSpec {
    Builtin(String),
    Named {
        name: String,
        region_args: Vec<String>,
        /// Type arguments on a named type use, e.g. `Option<i64>` (R11). Region
        /// arguments (`'r`) come first in the source list, then type arguments.
        type_args: Vec<ParsedTypeSpec>,
    },
    /// A bare name that `bind_type_params` has resolved to the enclosing generic
    /// definition's type parameter at this positional `index` (R11).
    TypeParam {
        index: u32,
    },
    Reference {
        region: String,
        mutable: bool,
        referent: Box<ParsedTypeSpec>,
    },
    RawPointer {
        mutable: bool,
        pointee: Box<ParsedTypeSpec>,
    },
    Box {
        element: Box<ParsedTypeSpec>,
    },
    Vec {
        element: Box<ParsedTypeSpec>,
    },
    String,
    Slice {
        region: String,
        mutable: bool,
        element: Box<ParsedTypeSpec>,
    },
    FixedArray {
        element: Box<ParsedTypeSpec>,
        len: u64,
    },
    Record(Vec<ParsedTypeField>),
    Enum(Vec<ParsedTypeField>),
}

impl ParsedTypeSpec {
    fn to_payload_spec(&self) -> Result<TypeSpec> {
        match self {
            ParsedTypeSpec::Builtin(kind) => Ok(TypeSpec::Builtin(kind.clone())),
            ParsedTypeSpec::Named { name, .. } => {
                bail!("named type {name} requires root-aware resolution")
            }
            ParsedTypeSpec::TypeParam { index } => Ok(TypeSpec::TypeParam { index: *index }),
            ParsedTypeSpec::Reference { region, .. } => {
                bail!("reference region '{region} requires root-aware resolution")
            }
            ParsedTypeSpec::RawPointer { mutable, pointee } => Ok(TypeSpec::RawPointer {
                mutable: *mutable,
                pointee: type_hash_for_spec(pointee)?,
            }),
            ParsedTypeSpec::Box { element } => Ok(TypeSpec::Box {
                element: type_hash_for_spec(element)?,
            }),
            ParsedTypeSpec::Vec { element } => Ok(TypeSpec::Vec {
                element: type_hash_for_spec(element)?,
            }),
            ParsedTypeSpec::String => Ok(TypeSpec::String),
            ParsedTypeSpec::Slice { region, .. } => {
                bail!("slice region '{region} requires root-aware resolution")
            }
            ParsedTypeSpec::FixedArray { element, len } => Ok(TypeSpec::FixedArray {
                element: type_hash_for_spec(element)?,
                len: *len,
            }),
            ParsedTypeSpec::Record(fields) => Ok(TypeSpec::Record(
                fields
                    .iter()
                    .map(|field| {
                        Ok(TypeFieldSpec {
                            name: field.name.clone(),
                            type_hash: type_hash_for_spec(&field.ty)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            )),
            ParsedTypeSpec::Enum(variants) => Ok(TypeSpec::Enum(
                variants
                    .iter()
                    .map(|variant| {
                        Ok(TypeFieldSpec {
                            name: variant.name.clone(),
                            type_hash: type_hash_for_spec(&variant.ty)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            )),
        }
    }
}

/// Collect the (possibly-qualified) type names a type definition's fields/variants
/// reference — used by the importer to detect mutually-recursive type cliques
/// (SPEC_V3 §6, D1). Parses each member's declared type and walks it for `Named`
/// references (region args are lifetimes, not types, so they are not followed).
pub(crate) fn collect_named_type_refs(definition: &TypeDefinitionKind) -> Result<Vec<String>> {
    let members = match definition {
        TypeDefinitionKind::Record { fields } => fields,
        TypeDefinitionKind::Enum { variants } => variants,
    };
    let mut names = Vec::new();
    for member in members {
        let parsed = parse_type_source(&member.ty)?;
        collect_parsed_named_refs(&parsed, &mut names);
    }
    Ok(names)
}

fn collect_parsed_named_refs(spec: &ParsedTypeSpec, out: &mut Vec<String>) {
    match spec {
        ParsedTypeSpec::Named {
            name, type_args, ..
        } => {
            out.push(name.clone());
            // A generic instance `Pair<List<i64>>` references both `Pair` and the
            // names in its type arguments (R11), so the clique analysis sees them.
            for arg in type_args {
                collect_parsed_named_refs(arg, out);
            }
        }
        ParsedTypeSpec::TypeParam { .. } => {}
        ParsedTypeSpec::Reference { referent, .. } => collect_parsed_named_refs(referent, out),
        ParsedTypeSpec::RawPointer { pointee, .. } => collect_parsed_named_refs(pointee, out),
        ParsedTypeSpec::Box { element }
        | ParsedTypeSpec::Vec { element }
        | ParsedTypeSpec::Slice { element, .. }
        | ParsedTypeSpec::FixedArray { element, .. } => collect_parsed_named_refs(element, out),
        ParsedTypeSpec::Record(fields) | ParsedTypeSpec::Enum(fields) => {
            for field in fields {
                collect_parsed_named_refs(&field.ty, out);
            }
        }
        ParsedTypeSpec::Builtin(_) | ParsedTypeSpec::String => {}
    }
}

/// A type definition's canonical structural form with in-clique peer type references
/// replaced by the peer's `colors` entry — the type analog of `recolor_peer_calls`
/// for the recursion-clique canonical labeling (SPEC_V3 §6, D1). Field/variant
/// display names are excluded (rename stays metadata-only); only member types, in
/// order, contribute. `name_to_local` maps each clique member's `(module, name)` to
/// its local index, so a peer reference resolves to its current colour.
pub(crate) fn recolor_type_definition_form(
    definition: &TypeDefinitionKind,
    module: &str,
    name_to_local: &std::collections::HashMap<(String, String), usize>,
    colors: &[String],
) -> String {
    let members = match definition {
        TypeDefinitionKind::Record { fields } => fields,
        TypeDefinitionKind::Enum { variants } => variants,
    };
    let forms: Vec<JsonValue> = members
        .iter()
        .map(|member| {
            let ty_form = match parse_type_source(&member.ty) {
                Ok(spec) => recolor_parsed_type(&spec, module, name_to_local, colors),
                // Already validated during clique analysis; fall back to the raw string.
                Err(_) => JsonValue::String(member.ty.clone()),
            };
            // Include the field/variant NAME alongside its (peer-recolored) type. A
            // member name is part of the stored `TypeDef`'s identity, so two members
            // that are structurally symmetric (same recolored type) but distinctly
            // named — e.g. `A.toB` / `B.toA` in an automorphic two-record clique —
            // must be DISCRETIZED by the canonical labeling. Without the name they
            // form one orbit the labeling cannot split, the member→ordinal mapping
            // falls back to source order, and because their names differ in the final
            // identity the group/root hash becomes source-order-dependent (breaking
            // content-addressing canonicality and the SPEC_V3 §11 round-trip). Peer
            // *references* stay erased (recolored above); only the intrinsic member
            // name — which the function-clique form omits because param names are
            // out-of-band metadata, but which IS identity for a type — is added.
            json!({ "name": member.name, "ty": ty_form })
        })
        .collect();
    canonical_json(&JsonValue::Array(forms))
}

fn recolor_parsed_type(
    spec: &ParsedTypeSpec,
    module: &str,
    name_to_local: &std::collections::HashMap<(String, String), usize>,
    colors: &[String],
) -> JsonValue {
    let recolor = |inner: &ParsedTypeSpec| recolor_parsed_type(inner, module, name_to_local, colors);
    match spec {
        ParsedTypeSpec::Builtin(kind) => json!({ "k": "builtin", "name": kind }),
        ParsedTypeSpec::String => json!({ "k": "string" }),
        ParsedTypeSpec::Named {
            name,
            region_args,
            type_args,
        } => {
            let head = match resolve_clique_type_name(name, module, name_to_local) {
                Some(local) => format!("@type-peer:{}", colors[local]),
                None => name.clone(),
            };
            json!({
                "k": "named",
                "name": head,
                "regions": region_args,
                "type_args": type_args.iter().map(&recolor).collect::<Vec<_>>(),
            })
        }
        ParsedTypeSpec::TypeParam { index } => json!({ "k": "type_param", "index": index }),
        ParsedTypeSpec::Reference {
            region,
            mutable,
            referent,
        } => json!({ "k": "ref", "region": region, "mut": mutable, "referent": recolor(referent) }),
        ParsedTypeSpec::RawPointer { mutable, pointee } => {
            json!({ "k": "raw", "mut": mutable, "pointee": recolor(pointee) })
        }
        ParsedTypeSpec::Box { element } => json!({ "k": "box", "elem": recolor(element) }),
        ParsedTypeSpec::Vec { element } => json!({ "k": "vec", "elem": recolor(element) }),
        ParsedTypeSpec::Slice {
            region,
            mutable,
            element,
        } => json!({ "k": "slice", "region": region, "mut": mutable, "elem": recolor(element) }),
        ParsedTypeSpec::FixedArray { element, len } => {
            json!({ "k": "array", "len": len, "elem": recolor(element) })
        }
        ParsedTypeSpec::Record(fields) => json!({
            "k": "record",
            "fields": fields.iter().map(|field| recolor(&field.ty)).collect::<Vec<_>>(),
        }),
        ParsedTypeSpec::Enum(fields) => json!({
            "k": "enum",
            "variants": fields.iter().map(|field| recolor(&field.ty)).collect::<Vec<_>>(),
        }),
    }
}

fn resolve_clique_type_name(
    name: &str,
    current_module: &str,
    name_to_local: &std::collections::HashMap<(String, String), usize>,
) -> Option<usize> {
    if let Some(dot) = name.rfind('.') {
        name_to_local
            .get(&(name[..dot].to_string(), name[dot + 1..].to_string()))
            .copied()
    } else {
        name_to_local
            .get(&(current_module.to_string(), name.to_string()))
            .copied()
    }
}

fn parse_type_source(source: &str) -> Result<ParsedTypeSpec> {
    let mut parser = TypeParser::new(source)?;
    let spec = parser.parse_type()?;
    parser.expect_eof()?;
    Ok(spec)
}

/// Rewrite a parsed type so a bare name matching one of `type_param_names` binds
/// to `ParsedTypeSpec::TypeParam { index }` at its positional index (R11). This
/// is the localized "type-parameter scope" — applied once to each member/param/
/// return type of a generic definition before root-aware resolution — so the
/// rest of the resolver needs no threaded scope. A type parameter may not take
/// arguments (`T<i64>` is rejected), since constraint-free generics are not
/// higher-kinded.
fn bind_type_params(spec: ParsedTypeSpec, type_param_names: &[String]) -> Result<ParsedTypeSpec> {
    let bind = |inner: ParsedTypeSpec| bind_type_params(inner, type_param_names);
    Ok(match spec {
        ParsedTypeSpec::Named {
            name,
            region_args,
            type_args,
        } => {
            if let Some(index) = type_param_names.iter().position(|candidate| *candidate == name) {
                if !region_args.is_empty() || !type_args.is_empty() {
                    bail!("type parameter {name} cannot take type or region arguments");
                }
                ParsedTypeSpec::TypeParam {
                    index: index as u32,
                }
            } else {
                ParsedTypeSpec::Named {
                    name,
                    region_args,
                    type_args: type_args
                        .into_iter()
                        .map(bind)
                        .collect::<Result<Vec<_>>>()?,
                }
            }
        }
        ParsedTypeSpec::TypeParam { index } => ParsedTypeSpec::TypeParam { index },
        ParsedTypeSpec::Reference {
            region,
            mutable,
            referent,
        } => ParsedTypeSpec::Reference {
            region,
            mutable,
            referent: Box::new(bind(*referent)?),
        },
        ParsedTypeSpec::RawPointer { mutable, pointee } => ParsedTypeSpec::RawPointer {
            mutable,
            pointee: Box::new(bind(*pointee)?),
        },
        ParsedTypeSpec::Box { element } => ParsedTypeSpec::Box {
            element: Box::new(bind(*element)?),
        },
        ParsedTypeSpec::Vec { element } => ParsedTypeSpec::Vec {
            element: Box::new(bind(*element)?),
        },
        ParsedTypeSpec::Slice {
            region,
            mutable,
            element,
        } => ParsedTypeSpec::Slice {
            region,
            mutable,
            element: Box::new(bind(*element)?),
        },
        ParsedTypeSpec::FixedArray { element, len } => ParsedTypeSpec::FixedArray {
            element: Box::new(bind(*element)?),
            len,
        },
        ParsedTypeSpec::Record(fields) => ParsedTypeSpec::Record(bind_type_param_fields(
            fields,
            type_param_names,
        )?),
        ParsedTypeSpec::Enum(variants) => ParsedTypeSpec::Enum(bind_type_param_fields(
            variants,
            type_param_names,
        )?),
        ParsedTypeSpec::Builtin(kind) => ParsedTypeSpec::Builtin(kind),
        ParsedTypeSpec::String => ParsedTypeSpec::String,
    })
}

fn bind_type_param_fields(
    fields: Vec<ParsedTypeField>,
    type_param_names: &[String],
) -> Result<Vec<ParsedTypeField>> {
    fields
        .into_iter()
        .map(|field| {
            Ok(ParsedTypeField {
                name: field.name,
                ty: bind_type_params(field.ty, type_param_names)?,
            })
        })
        .collect()
}

fn type_hash_for_spec(spec: &ParsedTypeSpec) -> Result<String> {
    match spec {
        ParsedTypeSpec::Builtin(kind) => Ok(type_hash_for(kind)),
        ParsedTypeSpec::Named { name, .. } => {
            bail!("named type {name} requires root-aware resolution")
        }
        ParsedTypeSpec::Reference { region, .. } | ParsedTypeSpec::Slice { region, .. } => {
            bail!("region '{region} requires root-aware resolution")
        }
        ParsedTypeSpec::TypeParam { .. }
        | ParsedTypeSpec::RawPointer { .. }
        | ParsedTypeSpec::Box { .. }
        | ParsedTypeSpec::Vec { .. }
        | ParsedTypeSpec::String
        | ParsedTypeSpec::FixedArray { .. }
        | ParsedTypeSpec::Record(_)
        | ParsedTypeSpec::Enum(_) => {
            let payload = type_payload_for_spec(&spec.to_payload_spec()?)?;
            Ok(hash_object_canonical(
                "Type",
                SCHEMA_VERSION,
                &canonical_json(&payload),
            ))
        }
    }
}

pub(crate) fn type_payload_for_spec(spec: &TypeSpec) -> Result<JsonValue> {
    Ok(match spec {
        TypeSpec::Builtin(kind) => json!({ "type_kind": kind }),
        TypeSpec::Named {
            type_symbol,
            region_args,
            type_args,
        } => {
            validate_region_args(region_args)?;
            for arg in type_args {
                validate_type_hash("named type argument", arg)?;
            }
            let mut payload = serde_json::Map::new();
            payload.insert("type_kind".to_string(), json!("Named"));
            payload.insert("type_symbol".to_string(), json!(type_symbol));
            payload.insert("region_args".to_string(), json!(region_args));
            // Emit `type_args` only when non-empty so a non-generic Named type's
            // payload — and therefore its content hash — is byte-identical to the
            // pre-generics form (the entire existing corpus keeps its hashes).
            if !type_args.is_empty() {
                payload.insert("type_args".to_string(), json!(type_args));
            }
            JsonValue::Object(payload)
        }
        TypeSpec::TypeParam { index } => json!({
            "type_kind": "TypeParam",
            "index": index,
        }),
        TypeSpec::Reference {
            region,
            mutable,
            referent,
        } => {
            validate_region_arg(region)?;
            validate_type_hash("reference referent", referent)?;
            json!({
                "type_kind": "Reference",
                "region": region,
                "mutable": mutable,
                "referent": referent,
            })
        }
        TypeSpec::RawPointer { mutable, pointee } => {
            validate_type_hash("raw pointer pointee", pointee)?;
            json!({
                "type_kind": "RawPointer",
                "mutable": mutable,
                "pointee": pointee,
            })
        }
        TypeSpec::Box { element } => {
            validate_type_hash("box element", element)?;
            json!({
                "type_kind": "Box",
                "element": element,
            })
        }
        TypeSpec::Vec { element } => {
            validate_type_hash("vec element", element)?;
            json!({
                "type_kind": "Vec",
                "element": element,
            })
        }
        TypeSpec::String => json!({
            "type_kind": "String",
        }),
        TypeSpec::Slice {
            region,
            mutable,
            element,
        } => {
            validate_region_arg(region)?;
            validate_type_hash("slice element", element)?;
            json!({
                "type_kind": "Slice",
                "region": region,
                "mutable": mutable,
                "element": element,
            })
        }
        TypeSpec::FixedArray { element, len } => {
            validate_type_hash("fixed array element", element)?;
            json!({
                "type_kind": "FixedArray",
                "element": element,
                "len": len,
            })
        }
        TypeSpec::Record(fields) => {
            let fields = canonical_type_fields("record field", fields)?;
            json!({
                "type_kind": "Record",
                "fields": fields
                    .into_iter()
                    .map(|field| json!({ "name": field.name, "type": field.type_hash }))
                    .collect::<Vec<_>>(),
            })
        }
        TypeSpec::Enum(variants) => {
            let variants = canonical_type_fields("enum variant", variants)?;
            json!({
                "type_kind": "Enum",
                "variants": variants
                    .into_iter()
                    .map(|variant| json!({ "name": variant.name, "type": variant.type_hash }))
                    .collect::<Vec<_>>(),
            })
        }
    })
}

pub(crate) fn type_spec_from_payload(payload: &JsonValue) -> Result<TypeSpec> {
    match payload
        .get("type_kind")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("Type object missing type_kind"))?
    {
        "Bool" => Ok(TypeSpec::Builtin("Bool".to_string())),
        "Unit" => Ok(TypeSpec::Builtin("Unit".to_string())),
        kind if scalar_int_type(kind).is_some() => Ok(TypeSpec::Builtin(kind.to_string())),
        "Named" => {
            let type_symbol = payload
                .get("type_symbol")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Named Type object missing type_symbol"))?
                .to_string();
            let region_args = match payload.get("region_args") {
                Some(JsonValue::Array(values)) => values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| anyhow!("Named Type region arg must be string"))
                    })
                    .collect::<Result<Vec<_>>>()?,
                Some(_) => bail!("Named Type region_args must be an array"),
                None => Vec::new(),
            };
            validate_region_args(&region_args)?;
            let type_args = match payload.get("type_args") {
                Some(JsonValue::Array(values)) => values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| anyhow!("Named Type type arg must be string"))
                    })
                    .collect::<Result<Vec<_>>>()?,
                Some(_) => bail!("Named Type type_args must be an array"),
                None => Vec::new(),
            };
            for arg in &type_args {
                validate_type_hash("named type argument", arg)?;
            }
            Ok(TypeSpec::Named {
                type_symbol,
                region_args,
                type_args,
            })
        }
        "TypeParam" => {
            let index = payload
                .get("index")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| anyhow!("TypeParam Type object missing index"))?;
            let index = u32::try_from(index)
                .map_err(|_| anyhow!("TypeParam index {index} out of range"))?;
            Ok(TypeSpec::TypeParam { index })
        }
        "Reference" => {
            let region = payload
                .get("region")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Reference Type object missing region"))?
                .to_string();
            validate_region_arg(&region)?;
            let mutable = payload
                .get("mutable")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| anyhow!("Reference Type object missing mutable"))?;
            let referent = payload
                .get("referent")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Reference Type object missing referent"))?
                .to_string();
            validate_type_hash("reference referent", &referent)?;
            Ok(TypeSpec::Reference {
                region,
                mutable,
                referent,
            })
        }
        "RawPointer" => {
            let mutable = payload
                .get("mutable")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| anyhow!("RawPointer Type object missing mutable"))?;
            let pointee = payload
                .get("pointee")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("RawPointer Type object missing pointee"))?
                .to_string();
            validate_type_hash("raw pointer pointee", &pointee)?;
            Ok(TypeSpec::RawPointer { mutable, pointee })
        }
        "Box" => {
            let element = payload
                .get("element")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Box Type object missing element"))?
                .to_string();
            validate_type_hash("box element", &element)?;
            Ok(TypeSpec::Box { element })
        }
        "Vec" => {
            let element = payload
                .get("element")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Vec Type object missing element"))?
                .to_string();
            validate_type_hash("vec element", &element)?;
            Ok(TypeSpec::Vec { element })
        }
        "String" => Ok(TypeSpec::String),
        "Slice" => {
            let region = payload
                .get("region")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Slice Type object missing region"))?
                .to_string();
            validate_region_arg(&region)?;
            let mutable = payload
                .get("mutable")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| anyhow!("Slice Type object missing mutable"))?;
            let element = payload
                .get("element")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("Slice Type object missing element"))?
                .to_string();
            validate_type_hash("slice element", &element)?;
            Ok(TypeSpec::Slice {
                region,
                mutable,
                element,
            })
        }
        "FixedArray" => {
            let element = payload
                .get("element")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("FixedArray Type object missing element"))?
                .to_string();
            validate_type_hash("fixed array element", &element)?;
            let len = payload
                .get("len")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| anyhow!("FixedArray Type object missing len"))?;
            Ok(TypeSpec::FixedArray { element, len })
        }
        "Record" => Ok(TypeSpec::Record(type_fields_from_payload(
            "record field",
            payload.get("fields"),
        )?)),
        "Enum" => Ok(TypeSpec::Enum(type_fields_from_payload(
            "enum variant",
            payload.get("variants"),
        )?)),
        other => bail!("unknown Type object kind {other}"),
    }
}

fn type_fields_from_payload(label: &str, value: Option<&JsonValue>) -> Result<Vec<TypeFieldSpec>> {
    let fields = value
        .and_then(JsonValue::as_array)
        .ok_or_else(|| anyhow!("{label}s must be an array"))?
        .iter()
        .map(|entry| {
            let name = entry
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("{label} missing name"))?
                .to_string();
            validate_projection_identifier(label, &name)?;
            let type_hash = entry
                .get("type")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("{label} missing type"))?
                .to_string();
            Ok(TypeFieldSpec { name, type_hash })
        })
        .collect::<Result<Vec<_>>>()?;
    canonical_type_fields(label, &fields)
}

fn canonical_type_fields(label: &str, fields: &[TypeFieldSpec]) -> Result<Vec<TypeFieldSpec>> {
    if fields.is_empty() {
        bail!("{label}s must not be empty");
    }
    let mut names = BTreeSet::new();
    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        validate_projection_identifier(label, &field.name)?;
        if !names.insert(field.name.clone()) {
            bail!("duplicate {label} {}", field.name);
        }
        out.push(field.clone());
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub(crate) fn validate_region_params(params: &[RegionParamDef]) -> Result<()> {
    let mut names = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    for param in params {
        validate_projection_identifier("region parameter", &param.name)?;
        if !names.insert(param.name.clone()) {
            bail!("duplicate region parameter {}", param.name);
        }
        if !param.region.starts_with("sha256:") {
            bail!("region parameter symbol must be a hash");
        }
        if !symbols.insert(param.region.clone()) {
            bail!("duplicate region parameter symbol {}", param.region);
        }
    }
    Ok(())
}

pub(crate) fn validate_member_defs(label: &str, members: &[TypeMemberDef]) -> Result<()> {
    if members.is_empty() {
        bail!("{label}s must not be empty");
    }
    let mut names = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    for member in members {
        validate_projection_identifier(label, &member.name)?;
        if !names.insert(member.name.clone()) {
            bail!("duplicate {label} {}", member.name);
        }
        if !member.member_symbol.starts_with("sha256:") {
            bail!("{label} symbol must be a hash");
        }
        if !symbols.insert(member.member_symbol.clone()) {
            bail!("duplicate {label} symbol {}", member.member_symbol);
        }
        if !member.type_hash.starts_with("sha256:") {
            bail!("{label} type must be a hash");
        }
    }
    Ok(())
}

pub(crate) fn validate_region_args(args: &[String]) -> Result<()> {
    for arg in args {
        validate_region_arg(arg)?;
    }
    Ok(())
}

fn validate_region_arg(arg: &str) -> Result<()> {
    if !arg.starts_with("sha256:") {
        bail!("region argument must be a region hash");
    }
    Ok(())
}

fn validate_region_name(label: &str, name: &str) -> Result<()> {
    if name == "static" {
        Ok(())
    } else {
        validate_projection_identifier(label, name)
    }
}

fn validate_type_hash(label: &str, hash: &str) -> Result<()> {
    if !hash.starts_with("sha256:") {
        bail!("{label} type must be a hash");
    }
    Ok(())
}

pub(crate) fn region_params_from_payload(value: Option<&JsonValue>) -> Result<Vec<RegionParamDef>> {
    let params = match value {
        Some(JsonValue::Array(values)) => values
            .iter()
            .map(|entry| {
                let region = entry
                    .get("region")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("region parameter missing region"))?
                    .to_string();
                let name = entry
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("region parameter missing name"))?
                    .to_string();
                Ok(RegionParamDef { region, name })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => bail!("region_params must be an array"),
        None => Vec::new(),
    };
    validate_region_params(&params)?;
    Ok(params)
}

pub(crate) fn type_params_from_payload(value: Option<&JsonValue>) -> Result<Vec<TypeParamDef>> {
    let params = match value {
        Some(JsonValue::Array(values)) => values
            .iter()
            .map(|entry| {
                let name = entry
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("type parameter missing name"))?
                    .to_string();
                Ok(TypeParamDef { name })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => bail!("type_params must be an array"),
        None => Vec::new(),
    };
    validate_type_params(&params)?;
    Ok(params)
}

pub(crate) fn validate_type_params(params: &[TypeParamDef]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for param in params {
        validate_projection_identifier("type parameter", &param.name)?;
        if !seen.insert(param.name.as_str()) {
            bail!("duplicate type parameter {}", param.name);
        }
    }
    Ok(())
}

/// Object kind for a generic function's monomorphic-instance descriptor (R11).
/// The instance's stable symbol is this object's content hash, so storing the
/// descriptor (see `monomorphic_instance_descriptor`) makes the symbol a real
/// content-addressed object — required because a root symbol references the
/// `objects` table — while keeping it a pure function of `(generic, type_args)`.
pub(crate) const MONOMORPHIC_INSTANCE_KIND: &str = "MonomorphicFunctionInstance";

/// Cap on `array<T, N>` lengths (#10). Fixed arrays are frame-allocated and
/// `[v; N]`/`array_set` lower to per-slot operations, so the length multiplies
/// evaluator memory AND lowered-IR size; an uncapped literal count
/// (`[0; u64::MAX]`) imported fine and then host-panicked eval ("capacity
/// overflow") or amplified gigabytes into the artifact store. 64Ki elements is
/// far beyond what the v0 frame layout can compile anyway (arm64 caps frames
/// at 4095 bytes) while keeping every pathological import bounded.
pub(crate) const MAX_FIXED_ARRAY_LEN: u64 = 65536;

/// Cap on a generic instantiation CHAIN (#7): the worklist depth for function
/// instances and the member-walk depth for type instances. Ordinary generic
/// recursion revisits the same instance and terminates via its `seen`/`done`
/// set; only polymorphic recursion (`f<T>` → `f<box<T>>`, `Grow<T>` containing
/// `Grow<box<T>>`) builds chains, and those never converge — so a depth past
/// this limit is rejected fail-closed instead of hanging the importer or
/// overflowing the host stack. Generous: legitimate nesting is single digits.
pub(crate) const GENERIC_INSTANTIATION_DEPTH_LIMIT: usize = 64;

/// The plain child-expression payload keys for an expression kind (R11) — the
/// single- and multi-child kinds. The leaves return no children; the kinds with
/// structured children (`call`, `record_literal`, `array_literal`, `case`) are
/// handled directly by the monomorphization traversals and never reach here. An
/// unknown kind fails closed.
/// Visit every CHILD EXPRESSION hash of one typed-DAG expression payload — the
/// single authority for "what are this node's subexpressions". Consumers that
/// walk the typed DAG (patch matching, bundle closure, blame/break-expr,
/// verify, reachability) MUST use this instead of a hand-maintained per-kind
/// match: six such walkers each drifted on a different subset of the V3.3
/// kinds (#12 — `loop`/`return` broke patching root-wide, vec/string programs
/// broke bundling). Plain single/multi-child kinds come from
/// [`plain_child_expr_keys`]; the four structured kinds (call args, record
/// fields, array elements, case scrutinee/guards/bodies) are enumerated here.
/// Unknown kinds FAIL CLOSED, so forgetting to extend the table for a new
/// expression kind is a loud error in every consumer, not a silent skip.
pub(crate) fn for_each_child_expr_hash(
    payload: &JsonValue,
    f: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    let kind = payload
        .get("expr_kind")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("expression missing expr_kind"))?;
    let child = |payload: &JsonValue, key: &str| -> Result<String> {
        Ok(payload
            .get(key)
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("{kind} missing {key}"))?
            .to_string())
    };
    match kind {
        "call" => {
            for arg in payload
                .get("args")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| anyhow!("call missing args"))?
            {
                f(arg
                    .as_str()
                    .ok_or_else(|| anyhow!("call arg must be hash"))?)?;
            }
        }
        "record_literal" => {
            for field in payload
                .get("fields")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| anyhow!("record_literal missing fields"))?
            {
                f(&child(field, "value")?)?;
            }
        }
        "array_literal" => {
            for element in payload
                .get("elements")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| anyhow!("array_literal missing elements"))?
            {
                f(&child(element, "value")?)?;
            }
        }
        "case" => {
            f(&child(payload, "expr")?)?;
            for arm in payload
                .get("arms")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| anyhow!("case missing arms"))?
            {
                // The guard (R14) is an ordinary child expression of its arm.
                if let Some(guard) = arm.get("guard").and_then(JsonValue::as_str) {
                    f(guard)?;
                }
                f(&child(arm, "body")?)?;
            }
        }
        other => {
            for key in plain_child_expr_keys(other)? {
                f(&child(payload, key)?)?;
            }
        }
    }
    Ok(())
}

/// Collect the child expression hashes of one typed-DAG node, in evaluation
/// order — the `Vec` form of [`for_each_child_expr_hash`].
pub(crate) fn child_expr_hashes(payload: &JsonValue) -> Result<Vec<String>> {
    let mut children = Vec::new();
    for_each_child_expr_hash(payload, &mut |hash| {
        children.push(hash.to_string());
        Ok(())
    })?;
    Ok(children)
}

pub(crate) fn plain_child_expr_keys(kind: &str) -> Result<&'static [&'static str]> {
    Ok(match kind {
        "literal_i64" | "literal_bool" | "literal_unit" | "static_bytes" | "param_ref"
        | "local_ref" | "arg_count" => &[],
        "unary" => &["expr"],
        "arg_len" => &["index"],
        "arg_byte" => &["index", "byte"],
        "int_cast" | "box_new" | "unbox" | "raw_ptr_cast" | "array_fill" | "enum_construct"
        | "return" => &["value"],
        "borrow_shared" | "borrow_mut" | "slice_from_array" | "slice_len" | "vec_len"
        | "string_len" | "field_access" => &["target"],
        "vec_new" | "string_with_capacity" => &["capacity"],
        "string_new" => &["source"],
        "raw_load" => &["pointer"],
        "binary" => &["left", "right"],
        "vec_push" | "string_push" | "assign" => &["target", "value"],
        "raw_store" => &["pointer", "value"],
        "vec_get" | "string_get" | "array_index" => &["target", "index"],
        "string_set" => &["target", "index", "value"],
        "array_set" => &["array", "index", "value"],
        "let" => &["value", "body"],
        "subslice" => &["target", "start", "len"],
        "if" => &["cond", "then", "else"],
        "fold" => &["target", "init", "body"],
        "loop" => &["init", "cond", "body"],
        other => bail!("unknown expression kind {other}"),
    })
}

/// The inferred type arguments recorded on a generic call expression (R11);
/// empty for a non-generic call (the `type_args` field is then absent).
pub(crate) fn call_type_args(payload: &JsonValue) -> Result<Vec<String>> {
    match payload.get("type_args") {
        None => Ok(Vec::new()),
        Some(JsonValue::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("call type arg must be a hash"))
            })
            .collect::<Result<Vec<_>>>(),
        Some(_) => bail!("call type_args must be an array"),
    }
}

/// The descriptor payload whose content hash is a generic instance's symbol
/// (R11): the generic symbol plus its concrete type arguments.
fn monomorphic_instance_descriptor(generic_symbol: &str, type_args: &[String]) -> JsonValue {
    json!({
        "generic": generic_symbol,
        "type_args": type_args,
    })
}

/// The derived stable symbol of a generic function's monomorphic instance
/// (R11): the content hash of its descriptor (`generic` + concrete
/// `type_args`). This hash *is* the instance's identity — its native ABI symbol
/// derives from it via `internal_abi_symbol`, so two call sites at the same type
/// share one instance and import→export→import reproduces it. Pure (no store),
/// so reachability, lowering, and monomorphization all derive the same symbol;
/// the descriptor object itself is stored by `build_function_instance` so the
/// symbol is a real object (a root symbol references the `objects` table).
pub(crate) fn monomorphic_instance_symbol(generic_symbol: &str, type_args: &[String]) -> String {
    let canonical = canonical_json(&monomorphic_instance_descriptor(generic_symbol, type_args));
    hash_object_canonical(MONOMORPHIC_INSTANCE_KIND, SCHEMA_VERSION, &canonical)
}

/// Validate a generic function's type-parameter *names* (R11): each must be a
/// valid identifier and they must be distinct. The string-list twin of
/// [`validate_type_params`], used on the function path where parameters are
/// carried by name on the signature rather than as `TypeParamDef`s.
pub(crate) fn validate_type_param_names(names: &[String]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        validate_projection_identifier("type parameter", name)?;
        if !seen.insert(name.as_str()) {
            bail!("duplicate type parameter {name}");
        }
    }
    Ok(())
}

pub(crate) fn member_defs_from_payload(
    label: &str,
    symbol_field: &str,
    value: Option<&JsonValue>,
) -> Result<Vec<TypeMemberDef>> {
    let members = value
        .and_then(JsonValue::as_array)
        .ok_or_else(|| anyhow!("{label}s must be an array"))?
        .iter()
        .map(|entry| {
            let member_symbol = entry
                .get(symbol_field)
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("{label} missing symbol"))?
                .to_string();
            let name = entry
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("{label} missing name"))?
                .to_string();
            let type_hash = entry
                .get("type")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("{label} missing type"))?
                .to_string();
            Ok(TypeMemberDef {
                member_symbol,
                name,
                type_hash,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    validate_member_defs(label, &members)?;
    Ok(members)
}

fn resolve_region_args(
    args: &[String],
    region_scope: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    args.iter()
        .map(|arg| resolve_region_arg(arg, region_scope))
        .collect()
}

fn resolve_region_arg(arg: &str, region_scope: &BTreeMap<String, String>) -> Result<String> {
    if arg == "static" {
        return Ok(static_region_hash());
    }
    region_scope
        .get(arg)
        .cloned()
        .ok_or_else(|| anyhow!("unknown region parameter '{arg}"))
}

fn hash_for_type_spec(spec: &TypeSpec) -> Result<String> {
    let payload = type_payload_for_spec(spec)?;
    Ok(hash_object_canonical(
        "Type",
        SCHEMA_VERSION,
        &canonical_json(&payload),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TypeToken {
    Ident(String),
    Number(String),
    Symbol(String),
    Eof,
}

struct TypeParser {
    tokens: Vec<TypeToken>,
    pos: usize,
}

impl TypeParser {
    fn new(source: &str) -> Result<Self> {
        Ok(Self {
            tokens: lex_type(source)?,
            pos: 0,
        })
    }

    fn parse_type(&mut self) -> Result<ParsedTypeSpec> {
        match self.next() {
            TypeToken::Symbol(value) if value == "&" => self.parse_reference_type(),
            TypeToken::Ident(value) if scalar_int_name_for_source(&value).is_some() => Ok(
                ParsedTypeSpec::Builtin(scalar_int_name_for_source(&value).unwrap().to_string()),
            ),
            TypeToken::Ident(value) if value == "bool" || value == "Bool" => {
                Ok(ParsedTypeSpec::Builtin("Bool".to_string()))
            }
            TypeToken::Ident(value) if value == "unit" || value == "Unit" => {
                Ok(ParsedTypeSpec::Builtin("Unit".to_string()))
            }
            TypeToken::Ident(value) if value == "string" || value == "String" => {
                Ok(ParsedTypeSpec::String)
            }
            TypeToken::Ident(value) if value == "record" => {
                Ok(ParsedTypeSpec::Record(self.parse_fields("record field")?))
            }
            TypeToken::Ident(value) if value == "enum" => {
                Ok(ParsedTypeSpec::Enum(self.parse_fields("enum variant")?))
            }
            TypeToken::Ident(value) if value == "raw_ptr" => Ok(ParsedTypeSpec::RawPointer {
                mutable: false,
                pointee: Box::new(self.parse_single_type_arg()?),
            }),
            TypeToken::Ident(value) if value == "raw_mut_ptr" => Ok(ParsedTypeSpec::RawPointer {
                mutable: true,
                pointee: Box::new(self.parse_single_type_arg()?),
            }),
            TypeToken::Ident(value) if value == "box" => Ok(ParsedTypeSpec::Box {
                element: Box::new(self.parse_single_type_arg()?),
            }),
            TypeToken::Ident(value) if value == "vec" => Ok(ParsedTypeSpec::Vec {
                element: Box::new(self.parse_single_type_arg()?),
            }),
            TypeToken::Ident(value) if value == "slice" => self.parse_slice_type(false),
            TypeToken::Ident(value) if value == "mut_slice" => self.parse_slice_type(true),
            TypeToken::Ident(value) if value == "array" => self.parse_fixed_array_type(),
            TypeToken::Ident(value) => {
                let name = self.finish_name_path(value)?;
                let (region_args, type_args) = self.parse_optional_type_args()?;
                Ok(ParsedTypeSpec::Named {
                    name,
                    region_args,
                    type_args,
                })
            }
            TypeToken::Symbol(value) if value == "(" => {
                self.expect_symbol(")")?;
                Ok(ParsedTypeSpec::Builtin("Unit".to_string()))
            }
            other => bail!("expected type, got {other:?}"),
        }
    }

    fn parse_reference_type(&mut self) -> Result<ParsedTypeSpec> {
        self.expect_symbol("'")?;
        let region = self.expect_ident()?;
        validate_region_name("reference region", &region)?;
        let mutable = self.consume_ident_value("mut");
        let referent = self.parse_type()?;
        Ok(ParsedTypeSpec::Reference {
            region,
            mutable,
            referent: Box::new(referent),
        })
    }

    fn parse_single_type_arg(&mut self) -> Result<ParsedTypeSpec> {
        self.expect_symbol("<")?;
        let ty = self.parse_type()?;
        self.expect_symbol(">")?;
        Ok(ty)
    }

    fn parse_slice_type(&mut self, mutable: bool) -> Result<ParsedTypeSpec> {
        self.expect_symbol("<")?;
        self.expect_symbol("'")?;
        let region = self.expect_ident()?;
        validate_region_name("slice region", &region)?;
        self.expect_symbol(",")?;
        let element = self.parse_type()?;
        self.expect_symbol(">")?;
        Ok(ParsedTypeSpec::Slice {
            region,
            mutable,
            element: Box::new(element),
        })
    }

    fn parse_fixed_array_type(&mut self) -> Result<ParsedTypeSpec> {
        self.expect_symbol("<")?;
        let element = self.parse_type()?;
        self.expect_symbol(",")?;
        let len = self.expect_number()?;
        self.expect_symbol(">")?;
        Ok(ParsedTypeSpec::FixedArray {
            element: Box::new(element),
            len,
        })
    }

    fn parse_fields(&mut self, label: &str) -> Result<Vec<ParsedTypeField>> {
        self.expect_symbol("{")?;
        let mut fields = Vec::new();
        if self.consume_symbol("}") {
            bail!("{label}s must not be empty");
        }
        loop {
            let name = self.expect_ident()?;
            validate_projection_identifier(label, &name)?;
            self.expect_symbol(":")?;
            let ty = self.parse_type()?;
            fields.push(ParsedTypeField { name, ty });
            if self.consume_symbol("}") {
                break;
            }
            self.expect_symbol(",")?;
        }
        validate_parsed_type_fields(label, fields)
    }

    /// Parse a named type's optional argument list `<...>` (R11). Region
    /// arguments (`'r`) come first, then type arguments (any type) —
    /// `Foo<'r, T1, T2>` — so the two lists round-trip from a single source list.
    /// A type argument may not follow... no, a region argument may not follow a
    /// type argument (regions precede types). The plain `Foo` form yields two
    /// empty lists.
    fn parse_optional_type_args(&mut self) -> Result<(Vec<String>, Vec<ParsedTypeSpec>)> {
        if !self.consume_symbol("<") {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut region_args = Vec::new();
        let mut type_args = Vec::new();
        if self.consume_symbol(">") {
            bail!("type/region argument list must not be empty");
        }
        loop {
            if self.peek_symbol("'") {
                if !type_args.is_empty() {
                    bail!("region arguments must come before type arguments");
                }
                self.expect_symbol("'")?;
                let name = self.expect_ident()?;
                validate_region_name("region argument", &name)?;
                region_args.push(name);
            } else {
                type_args.push(self.parse_type()?);
            }
            if self.consume_symbol(">") {
                break;
            }
            self.expect_symbol(",")?;
        }
        Ok((region_args, type_args))
    }

    fn finish_name_path(&mut self, first: String) -> Result<String> {
        let mut parts = vec![first];
        while self.consume_symbol(".") {
            parts.push(self.expect_ident()?);
        }
        Ok(parts.join("."))
    }

    fn expect_eof(&self) -> Result<()> {
        if matches!(self.peek(), TypeToken::Eof) {
            Ok(())
        } else {
            bail!("unexpected token at end of type: {:?}", self.peek())
        }
    }

    fn expect_ident(&mut self) -> Result<String> {
        match self.next() {
            TypeToken::Ident(value) => Ok(value),
            other => bail!("expected identifier, got {other:?}"),
        }
    }

    fn expect_number(&mut self) -> Result<u64> {
        match self.next() {
            TypeToken::Number(value) => value
                .parse::<u64>()
                .with_context(|| format!("invalid array length {value}")),
            other => bail!("expected number, got {other:?}"),
        }
    }

    fn expect_symbol(&mut self, expected: &str) -> Result<()> {
        match self.next() {
            TypeToken::Symbol(value) if value == expected => Ok(()),
            other => bail!("expected symbol {expected}, got {other:?}"),
        }
    }

    fn consume_symbol(&mut self, expected: &str) -> bool {
        match self.peek() {
            TypeToken::Symbol(value) if value == expected => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn peek_symbol(&self, expected: &str) -> bool {
        matches!(self.peek(), TypeToken::Symbol(value) if value == expected)
    }

    fn consume_ident_value(&mut self, expected: &str) -> bool {
        match self.peek() {
            TypeToken::Ident(value) if value == expected => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn peek(&self) -> &TypeToken {
        self.tokens.get(self.pos).unwrap_or(&TypeToken::Eof)
    }

    fn next(&mut self) -> TypeToken {
        let token = self.tokens.get(self.pos).cloned().unwrap_or(TypeToken::Eof);
        if !matches!(token, TypeToken::Eof) {
            self.pos += 1;
        }
        token
    }
}

fn validate_parsed_type_fields(
    label: &str,
    mut fields: Vec<ParsedTypeField>,
) -> Result<Vec<ParsedTypeField>> {
    let mut names = BTreeSet::new();
    for field in &fields {
        if !names.insert(field.name.clone()) {
            bail!("duplicate {label} {}", field.name);
        }
    }
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(fields)
}

fn lex_type(source: &str) -> Result<Vec<TypeToken>> {
    let mut tokens = Vec::new();
    let chars = source.chars().collect::<Vec<_>>();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(TypeToken::Ident(chars[start..i].iter().collect()));
        } else if ch.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            tokens.push(TypeToken::Number(chars[start..i].iter().collect()));
        } else {
            tokens.push(TypeToken::Symbol(ch.to_string()));
            i += 1;
        }
    }
    tokens.push(TypeToken::Eof);
    Ok(tokens)
}
