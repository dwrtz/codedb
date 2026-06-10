use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::backend::ArtifactKind;
use crate::expr::{RawCaseArm, RawExpr};
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
        fields: Vec<TypeMemberDef>,
    },
    Enum {
        type_symbol: String,
        region_params: Vec<RegionParamDef>,
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

#[derive(Debug, Clone)]
struct MoveBorrowState {
    locals: Vec<usize>,
    active: Vec<ActiveLoan>,
    moved: Vec<LoanPlace>,
    next_local: usize,
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

impl CodeDb {
    pub(crate) fn insert_builtin_types(&mut self) -> Result<()> {
        for type_name in ["I64", "Bool", "Unit", "U8"] {
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
            fields,
        } = definition
        else {
            bail!("put_record_def requires record definition");
        };
        validate_region_params(region_params)?;
        validate_member_defs("record field", fields)?;
        self.put_object(
            "RecordDef",
            &json!({
                "type_symbol": type_symbol,
                "region_params": region_params
                    .iter()
                    .map(|param| json!({ "region": param.region, "name": param.name }))
                    .collect::<Vec<_>>(),
                "fields": fields
                    .iter()
                    .map(|field| {
                        json!({
                            "field_symbol": field.member_symbol,
                            "name": field.name,
                            "type": field.type_hash,
                        })
                    })
                    .collect::<Vec<_>>(),
            }),
        )
    }

    pub(crate) fn put_enum_def(&mut self, definition: &TypeDefinition) -> Result<String> {
        let TypeDefinition::Enum {
            type_symbol,
            region_params,
            variants,
        } = definition
        else {
            bail!("put_enum_def requires enum definition");
        };
        validate_region_params(region_params)?;
        validate_member_defs("enum variant", variants)?;
        self.put_object(
            "EnumDef",
            &json!({
                "type_symbol": type_symbol,
                "region_params": region_params
                    .iter()
                    .map(|param| json!({ "region": param.region, "name": param.name }))
                    .collect::<Vec<_>>(),
                "variants": variants
                    .iter()
                    .map(|variant| {
                        json!({
                            "variant_symbol": variant.member_symbol,
                            "name": variant.name,
                            "type": variant.type_hash,
                        })
                    })
                    .collect::<Vec<_>>(),
            }),
        )
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
        let fields =
            member_defs_from_payload("record field", "field_symbol", payload.get("fields"))?;
        validate_region_params(&region_params)?;
        validate_member_defs("record field", &fields)?;
        Ok(TypeDefinition::Record {
            type_symbol,
            region_params,
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
        let variants =
            member_defs_from_payload("enum variant", "variant_symbol", payload.get("variants"))?;
        validate_region_params(&region_params)?;
        validate_member_defs("enum variant", &variants)?;
        Ok(TypeDefinition::Enum {
            type_symbol,
            region_params,
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
        let parsed = parse_type_source(ty)?;
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
        let parsed = parse_type_source(ty)?;
        self.type_hash_for_parsed_in_root(current_module, root, &parsed, region_scope)
    }

    pub(crate) fn type_name(&self, hash: &str) -> Result<String> {
        if hash == type_hash_for("I64") {
            Ok("i64".to_string())
        } else if hash == type_hash_for("U8") {
            Ok("u8".to_string())
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
        if hash == type_hash_for("I64") {
            return Ok("i64".to_string());
        }
        if hash == type_hash_for("U8") {
            return Ok("u8".to_string());
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

    pub(crate) fn type_name_in_root_with_regions(
        &self,
        root: &ProgramRootPayload,
        current_module: &str,
        hash: &str,
        region_names: &BTreeMap<String, String>,
    ) -> Result<String> {
        if hash == type_hash_for("I64") {
            return Ok("i64".to_string());
        }
        if hash == type_hash_for("U8") {
            return Ok("u8".to_string());
        }
        if hash == type_hash_for("Bool") {
            return Ok("bool".to_string());
        }
        if hash == type_hash_for("Unit") {
            return Ok("unit".to_string());
        }
        match self.type_spec(hash)? {
            TypeSpec::Named {
                type_symbol,
                region_args,
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
                let referent = self.type_name_in_root_with_regions(
                    root,
                    current_module,
                    &referent,
                    region_names,
                )?;
                if mutable {
                    Ok(format!("&'{region_name} mut {referent}"))
                } else {
                    Ok(format!("&'{region_name} {referent}"))
                }
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                let pointee = self.type_name_in_root_with_regions(
                    root,
                    current_module,
                    &pointee,
                    region_names,
                )?;
                if mutable {
                    Ok(format!("raw_mut_ptr<{pointee}>"))
                } else {
                    Ok(format!("raw_ptr<{pointee}>"))
                }
            }
            TypeSpec::Box { element } => {
                let element = self.type_name_in_root_with_regions(
                    root,
                    current_module,
                    &element,
                    region_names,
                )?;
                Ok(format!("box<{element}>"))
            }
            TypeSpec::Vec { element } => {
                let element = self.type_name_in_root_with_regions(
                    root,
                    current_module,
                    &element,
                    region_names,
                )?;
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
                let element = self.type_name_in_root_with_regions(
                    root,
                    current_module,
                    &element,
                    region_names,
                )?;
                if mutable {
                    Ok(format!("mut_slice<'{region_name}, {element}>"))
                } else {
                    Ok(format!("slice<'{region_name}, {element}>"))
                }
            }
            TypeSpec::FixedArray { element, len } => {
                let element = self.type_name_in_root_with_regions(
                    root,
                    current_module,
                    &element,
                    region_names,
                )?;
                Ok(format!("array<{element}, {len}>"))
            }
            TypeSpec::Record(fields) => {
                let rendered = fields
                    .iter()
                    .map(|field| {
                        Ok(format!(
                            "{}: {}",
                            field.name,
                            self.type_name_in_root_with_regions(
                                root,
                                current_module,
                                &field.type_hash,
                                region_names,
                            )?
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
                            self.type_name_in_root_with_regions(
                                root,
                                current_module,
                                &variant.type_hash,
                                region_names,
                            )?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("enum {{{}}}", rendered.join(", ")))
            }
            TypeSpec::Builtin(_) => self.type_name(hash),
        }
    }

    pub(crate) fn type_spec(&self, hash: &str) -> Result<TypeSpec> {
        if hash == type_hash_for("I64") {
            return Ok(TypeSpec::Builtin("I64".to_string()));
        }
        if hash == type_hash_for("U8") {
            return Ok(TypeSpec::Builtin("U8".to_string()));
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
            } => {
                let (definition, region_substitutions) =
                    self.named_type_definition_with_regions(root, &type_symbol, &region_args)?;
                match definition {
                    TypeDefinition::Record { fields, .. } => Ok(TypeSpec::Record(
                        fields
                            .into_iter()
                            .map(|field| {
                                Ok(TypeFieldSpec {
                                    name: field.name,
                                    type_hash: self.substitute_type_regions_hash(
                                        &field.type_hash,
                                        &region_substitutions,
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
                                    type_hash: self.substitute_type_regions_hash(
                                        &variant.type_hash,
                                        &region_substitutions,
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

    fn named_type_definition_with_regions(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        region_args: &[String],
    ) -> Result<(TypeDefinition, BTreeMap<String, String>)> {
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
        let region_substitutions = definition
            .region_params()
            .iter()
            .zip(region_args.iter())
            .map(|(param, arg)| (param.region.clone(), arg.clone()))
            .collect();
        Ok((definition, region_substitutions))
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
        if region_substitutions.is_empty() {
            return Ok(type_hash.to_string());
        }
        match self.type_spec(type_hash)? {
            TypeSpec::Builtin(_) => Ok(type_hash.to_string()),
            TypeSpec::Named {
                type_symbol,
                region_args,
            } => hash_for_type_spec(&TypeSpec::Named {
                type_symbol,
                region_args: region_args
                    .into_iter()
                    .map(|region| self.substitute_region_hash(region, region_substitutions))
                    .collect(),
            }),
            TypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let referent =
                    self.substitute_type_regions_hash(&referent, region_substitutions)?;
                hash_for_type_spec(&TypeSpec::Reference {
                    region: self.substitute_region_hash(region, region_substitutions),
                    mutable,
                    referent,
                })
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                let pointee = self.substitute_type_regions_hash(&pointee, region_substitutions)?;
                hash_for_type_spec(&TypeSpec::RawPointer { mutable, pointee })
            }
            TypeSpec::Box { element } => {
                let element = self.substitute_type_regions_hash(&element, region_substitutions)?;
                hash_for_type_spec(&TypeSpec::Box { element })
            }
            TypeSpec::Vec { element } => {
                let element = self.substitute_type_regions_hash(&element, region_substitutions)?;
                hash_for_type_spec(&TypeSpec::Vec { element })
            }
            TypeSpec::String => hash_for_type_spec(&TypeSpec::String),
            TypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let element = self.substitute_type_regions_hash(&element, region_substitutions)?;
                hash_for_type_spec(&TypeSpec::Slice {
                    region: self.substitute_region_hash(region, region_substitutions),
                    mutable,
                    element,
                })
            }
            TypeSpec::FixedArray { element, len } => {
                let element = self.substitute_type_regions_hash(&element, region_substitutions)?;
                hash_for_type_spec(&TypeSpec::FixedArray { element, len })
            }
            TypeSpec::Record(fields) => {
                let fields = fields
                    .into_iter()
                    .map(|field| {
                        Ok(TypeFieldSpec {
                            name: field.name,
                            type_hash: self.substitute_type_regions_hash(
                                &field.type_hash,
                                region_substitutions,
                            )?,
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
                            type_hash: self.substitute_type_regions_hash(
                                &variant.type_hash,
                                region_substitutions,
                            )?,
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
        if region_substitutions.is_empty() {
            return Ok(type_hash.to_string());
        }
        match self.type_spec(type_hash)? {
            TypeSpec::Builtin(_) => Ok(type_hash.to_string()),
            TypeSpec::Named {
                type_symbol,
                region_args,
            } => self.put_structural_type(TypeSpec::Named {
                type_symbol,
                region_args: region_args
                    .into_iter()
                    .map(|region| self.substitute_region_hash(region, region_substitutions))
                    .collect(),
            }),
            TypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                let referent =
                    self.put_substituted_type_regions(&referent, region_substitutions)?;
                self.put_structural_type(TypeSpec::Reference {
                    region: self.substitute_region_hash(region, region_substitutions),
                    mutable,
                    referent,
                })
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                let pointee = self.put_substituted_type_regions(&pointee, region_substitutions)?;
                self.put_structural_type(TypeSpec::RawPointer { mutable, pointee })
            }
            TypeSpec::Box { element } => {
                let element = self.put_substituted_type_regions(&element, region_substitutions)?;
                self.put_structural_type(TypeSpec::Box { element })
            }
            TypeSpec::Vec { element } => {
                let element = self.put_substituted_type_regions(&element, region_substitutions)?;
                self.put_structural_type(TypeSpec::Vec { element })
            }
            TypeSpec::String => self.put_structural_type(TypeSpec::String),
            TypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let element = self.put_substituted_type_regions(&element, region_substitutions)?;
                self.put_structural_type(TypeSpec::Slice {
                    region: self.substitute_region_hash(region, region_substitutions),
                    mutable,
                    element,
                })
            }
            TypeSpec::FixedArray { element, len } => {
                let element = self.put_substituted_type_regions(&element, region_substitutions)?;
                self.put_structural_type(TypeSpec::FixedArray { element, len })
            }
            TypeSpec::Record(fields) => {
                let fields = fields
                    .into_iter()
                    .map(|field| {
                        Ok(TypeFieldSpec {
                            name: field.name,
                            type_hash: self.put_substituted_type_regions(
                                &field.type_hash,
                                region_substitutions,
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
                            type_hash: self.put_substituted_type_regions(
                                &variant.type_hash,
                                region_substitutions,
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
    ) -> Result<()> {
        let (definition, region_substitutions) =
            self.named_type_definition_with_regions(root, type_symbol, region_args)?;
        match definition {
            TypeDefinition::Record { fields, .. } => {
                for field in fields {
                    self.put_substituted_type_regions(&field.type_hash, &region_substitutions)?;
                }
            }
            TypeDefinition::Enum { variants, .. } => {
                for variant in variants {
                    self.put_substituted_type_regions(&variant.type_hash, &region_substitutions)?;
                }
            }
        }
        Ok(())
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
                },
                TypeSpec::Named {
                    type_symbol: expected_symbol,
                    region_args: expected_args,
                },
            ) => {
                if actual_symbol != expected_symbol || actual_args.len() != expected_args.len() {
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
            },
            TypeSpec::Named {
                type_symbol: expected_symbol,
                region_args: expected_args,
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
            ParsedTypeSpec::Named { name, region_args } => {
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
                let region_args = resolve_region_args(region_args, region_scope)?;
                let type_hash = self.put_structural_type(TypeSpec::Named {
                    type_symbol: type_symbol.clone(),
                    region_args: region_args.clone(),
                })?;
                self.materialize_named_type_expansion(root, &type_symbol, &region_args)?;
                Ok(type_hash)
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
            ParsedTypeSpec::Named { name, region_args } => {
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
                hash_for_type_spec(&TypeSpec::Named {
                    type_symbol,
                    region_args: resolve_region_args(region_args, region_scope)?,
                })
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
        payload.insert("params".to_string(), json!(param_types));
        payload.insert("return".to_string(), json!(return_type));
        payload.insert("abi".to_string(), json!(ABI_TAG));
        payload.insert("effects".to_string(), json!(effect_names(&effects)));
        self.put_object("FunctionSignature", &JsonValue::Object(payload))
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
            TypeSpec::Builtin(_) | TypeSpec::Named { .. } => Ok(false),
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
                value
                    .parse::<i64>()
                    .with_context(|| format!("invalid i64 literal {value}"))?;
                let type_hash = type_hash_for("I64");
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
                    "vec_new" | "vec_push" | "vec_get" | "vec_len" | "string_new" | "string_len"
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
                let mut typed_args = Vec::with_capacity(args.len());
                let callee_regions = self
                    .signature_region_params(&callee.signature)?
                    .into_iter()
                    .map(|param| param.region)
                    .collect::<BTreeSet<_>>();
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
                let left = self.type_expr_with_locals(
                    current_module,
                    left,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let right = self.type_expr_with_locals(
                    current_module,
                    right,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let bool_hash = type_hash_for("Bool");
                let result_type = match op.as_str() {
                    "+" | "-" | "*" | "/" => {
                        let i64_hash = type_hash_for("I64");
                        require_type(&left.type_hash, &i64_hash, "left operand", self)?;
                        require_type(&right.type_hash, &i64_hash, "right operand", self)?;
                        i64_hash
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        if left.type_hash != right.type_hash {
                            bail!(
                                "comparison operands differ: {} vs {}",
                                self.type_name(&left.type_hash)?,
                                self.type_name(&right.type_hash)?
                            );
                        }
                        if left.type_hash != type_hash_for("I64")
                            && left.type_hash != type_hash_for("U8")
                        {
                            bail!(
                                "comparison operand expected i64 or u8, got {}",
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
                let typed = self.type_expr_with_locals(
                    current_module,
                    expr,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                let i64_hash = type_hash_for("I64");
                let bool_hash = type_hash_for("Bool");
                let result_type = match op.as_str() {
                    "-" => {
                        require_type(&typed.type_hash, &i64_hash, "unary operand", self)?;
                        i64_hash
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
                if then_expr.type_hash != else_expr.type_hash {
                    bail!(
                        "if branches differ: {} vs {}",
                        self.type_name(&then_expr.type_hash)?,
                        self.type_name(&else_expr.type_hash)?
                    );
                }
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "if",
                        "cond": cond.expr_hash,
                        "then": then_expr.expr_hash,
                        "else": else_expr.expr_hash,
                        "type": then_expr.type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    ArtifactKind::TypedExpression,
                    &json!({ "type": then_expr.type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: then_expr.type_hash,
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
            RawExpr::Array { elements } => {
                if elements.is_empty() {
                    bail!("array literal must have at least one element");
                }
                let mut typed_elements = Vec::with_capacity(elements.len());
                for element in elements {
                    typed_elements.push(self.type_expr_with_locals(
                        current_module,
                        element,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
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
                let mut names = BTreeSet::new();
                let mut typed_values = Vec::with_capacity(fields.len());
                for field in fields {
                    validate_projection_identifier("record field", &field.name)?;
                    if !names.insert(field.name.clone()) {
                        bail!("duplicate record field {}", field.name);
                    }
                    let typed = self.type_expr_with_locals(
                        current_module,
                        &field.value,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
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
                let enum_type_hash = self.resolve_type_in_root_with_regions(
                    current_module,
                    root,
                    enum_type,
                    region_scope,
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
                let mut result_type: Option<String> = None;
                let mut arms_json = Vec::with_capacity(arms.len());
                let mut has_default = false;
                for (index, arm) in arms.iter().enumerate() {
                    let mut binding_was_pushed = false;
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
                    } else {
                        let variant = arm
                            .variant
                            .as_deref()
                            .ok_or_else(|| anyhow!("case arm missing variant"))?;
                        validate_projection_identifier("enum variant", variant)?;
                        if !seen.insert(variant.to_string()) {
                            bail!("duplicate case arm {variant}");
                        }
                        let variant_type = variants
                            .iter()
                            .find(|candidate| candidate.name == variant)
                            .map(|candidate| candidate.type_hash.clone())
                            .ok_or_else(|| anyhow!("case arm uses unknown variant {variant}"))?;
                        if let Some(binding) = &arm.binding {
                            validate_projection_identifier("case binding", binding)?;
                            locals.push(LocalTypeBinding {
                                name: binding.clone(),
                                type_hash: variant_type.clone(),
                            });
                            binding_was_pushed = true;
                        } else if variant_type != type_hash_for("Unit") {
                            bail!("case arm {variant} must bind its payload");
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
                        if binding_was_pushed {
                            locals.pop();
                        }
                        body = typed_body?;
                    }
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
                    if arm.default {
                        arms_json.push(json!({
                            "default": true,
                            "body": body.expr_hash,
                        }));
                    } else {
                        arms_json.push(json!({
                            "variant": arm.variant.as_deref(),
                            "binding_name": &arm.binding,
                            "body": body.expr_hash,
                        }));
                    }
                }
                let expected_variants = variants
                    .iter()
                    .map(|variant| variant.name.clone())
                    .collect::<BTreeSet<_>>();
                if !has_default && seen != expected_variants {
                    bail!("case expression must cover every enum variant");
                }
                let type_hash =
                    result_type.ok_or_else(|| anyhow!("case expression has no arms"))?;
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
            let mut pattern = serde_json::Map::new();
            if arm.default {
                if index + 1 != arms.len() {
                    bail!("default case arm must be last");
                }
                if has_default {
                    bail!("duplicate default case arm");
                }
                has_default = true;
                pattern.insert("default".to_string(), json!(true));
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
                        if !seen_i64.insert(value.clone()) {
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
            let body = self.type_expr_with_locals(
                current_module,
                &arm.body,
                root,
                param_names,
                param_types,
                region_scope,
                locals,
            )?;
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
        let type_hash = result_type.ok_or_else(|| anyhow!("case expression has no arms"))?;
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
            self.signature_effects(&entry.signature)?;
            for param_type in &param_types {
                self.validate_type_hash_in_root(&root, param_type, &allowed_regions)?;
            }
            self.validate_type_hash_in_root(&root, &return_type, &allowed_regions)?;
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
            let actual = self.verify_expr_type(&body, &root, &param_types, &allowed_regions)?;
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
            "vec_new" | "vec_push" | "vec_get" | "vec_len" | "string_new" | "string_len" => {
                Ok(false)
            }
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
            "fold" => Ok(false),
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
                    if arm
                        .get("binding_name")
                        .is_some_and(|value| !value.is_null())
                    {
                        locals_with_local_borrows.push(scrutinee_has_local_borrow);
                    }
                    let body_result =
                        self.expr_escapes_local_borrow(root, body_hash, locals_with_local_borrows);
                    if arm
                        .get("binding_name")
                        .is_some_and(|value| !value.is_null())
                    {
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
            "binary" | "unary" => Ok(false),
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
        let (expected_params, return_type) = self.signature_parts(&callee.signature)?;
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
            match definition {
                TypeDefinition::Record { fields, .. } => {
                    for field in fields {
                        self.validate_type_hash_in_root(root, &field.type_hash, &allowed_regions)?;
                    }
                }
                TypeDefinition::Enum { variants, .. } => {
                    for variant in variants {
                        self.validate_type_hash_in_root(
                            root,
                            &variant.type_hash,
                            &allowed_regions,
                        )?;
                    }
                }
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
        match self.type_spec(type_hash)? {
            TypeSpec::Builtin(_) => Ok(()),
            TypeSpec::Named {
                type_symbol,
                region_args,
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
                for region in region_args {
                    if !allowed_regions.contains(&region) && !is_static_region(&region) {
                        bail!("invalid region reference {region}");
                    }
                }
                Ok(())
            }
            TypeSpec::Reference {
                region, referent, ..
            } => {
                if !allowed_regions.contains(&region) && !is_static_region(&region) {
                    bail!("invalid region reference {region}");
                }
                self.validate_type_hash_in_root(root, &referent, allowed_regions)
            }
            TypeSpec::RawPointer { pointee, .. } => {
                self.validate_type_hash_in_root(root, &pointee, allowed_regions)
            }
            TypeSpec::Box { element } => {
                self.validate_type_hash_in_root(root, &element, allowed_regions)
            }
            TypeSpec::Vec { element } => {
                self.validate_type_hash_in_root(root, &element, allowed_regions)
            }
            TypeSpec::String => Ok(()),
            TypeSpec::Slice {
                region, element, ..
            } => {
                if !allowed_regions.contains(&region) && !is_static_region(&region) {
                    bail!("invalid region reference {region}");
                }
                self.validate_type_hash_in_root(root, &element, allowed_regions)
            }
            TypeSpec::FixedArray { element, .. } => {
                self.validate_type_hash_in_root(root, &element, allowed_regions)
            }
            TypeSpec::Record(fields) | TypeSpec::Enum(fields) => {
                for field in fields {
                    self.validate_type_hash_in_root(root, &field.type_hash, allowed_regions)?;
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
                merge_branch_state(state, then_state, else_state);
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
                let body_result =
                    self.verify_expr_borrows(root, body, param_types, state, ExprUse::Value);
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
                    let pushed = arm
                        .get("binding_name")
                        .and_then(JsonValue::as_str)
                        .is_some();
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
                        self.verify_expr_borrows(
                            root,
                            body,
                            param_types,
                            &mut arm_state,
                            ExprUse::Value,
                        )?;
                    }
                    merged = Some(match merged {
                        Some(previous) => merged_branch_states(previous, arm_state),
                        None => arm_state,
                    });
                }
                if let Some(merged) = merged {
                    *state = merged;
                }
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
                    if arm
                        .get("binding_name")
                        .and_then(JsonValue::as_str)
                        .is_some()
                    {
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
                    if arm
                        .get("binding_name")
                        .and_then(JsonValue::as_str)
                        .is_some()
                    {
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
            // Field-granular drop glue (SPEC_V3 §7): a partial move out of a
            // record field is supported — lowering drops the live remainder of
            // the enclosing aggregate while skipping the moved-out field. Moving
            // out of an array element (`[N]`/`[*]` path segment) or behind a
            // pointer is not field-granular-droppable, so it stays fail-closed.
            if place.fields.iter().any(|segment| segment.starts_with('[')) {
                bail!(
                    "unsupported_move: partial move of an owned array element at {:?}; field-granular drop glue covers record fields only (SPEC_V3 §7)",
                    place
                );
            }
            // A partial move out of a field reached through a `box` auto-deref (e.g.
            // `h.inner` where `h: box<Holder>`) is not field-granular-droppable: the
            // box's whole-slot drop glue cannot skip a moved-out interior field, and
            // the place is flattened (the deref is invisible in `place`). Reject it
            // cleanly here rather than crashing lowering (SPEC_V3 §7; `unbox` first if
            // you need to move the payload out).
            if self.move_crosses_box_deref(expr_hash)? {
                bail!(
                    "unsupported_move: partial move of an owned value reached through a box deref at {:?}; field-granular drop glue covers record fields rooted in a local or parameter, not behind a box — `unbox` the box first (SPEC_V3 §7)",
                    place
                );
            }
            self.check_move_conflicts(&place, &state.active)?;
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
    /// Whether reaching this place requires dereferencing a `box` (auto-deref). Such
    /// a partial move is not field-granular-droppable (SPEC_V3 §7), so the move-check
    /// rejects it. Walks the field/index access chain; true as soon as any access
    /// target is a `box<T>`.
    fn move_crosses_box_deref(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("field_access") => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                let target_type = self.expr_declared_type(target)?;
                if matches!(self.type_spec(&target_type)?, TypeSpec::Box { .. }) {
                    return Ok(true);
                }
                self.move_crosses_box_deref(target)
            }
            Some("array_index") => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                self.move_crosses_box_deref(target)
            }
            _ => Ok(false),
        }
    }

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
                "raw_ptr_cast" => self.expr_child_requires_state(&payload, "value")?,
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
                "fold" => {
                    self.expr_child_requires_state(&payload, "target")?
                        || self.expr_child_requires_state(&payload, "init")?
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
                // `unbox` frees the box shell, so it requires `alloc` just like the
                // allocating builtins (the deallocation is in the alloc effect domain).
                "box_new" | "unbox" | "vec_new" | "string_new" => true,
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
                "raw_ptr_cast" => self.expr_child_requires_alloc(&payload, "value")?,
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
                "let" => {
                    self.expr_child_requires_alloc(&payload, "value")?
                        || self.expr_child_requires_alloc(&payload, "body")?
                }
                "if" => {
                    self.expr_child_requires_alloc(&payload, "cond")?
                        || self.expr_child_requires_alloc(&payload, "then")?
                        || self.expr_child_requires_alloc(&payload, "else")?
                }
                "fold" => {
                    self.expr_child_requires_alloc(&payload, "target")?
                        || self.expr_child_requires_alloc(&payload, "init")?
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
                "let" => {
                    self.expr_child_requires_unsafe(&payload, "value")?
                        || self.expr_child_requires_unsafe(&payload, "body")?
                }
                "if" => {
                    self.expr_child_requires_unsafe(&payload, "cond")?
                        || self.expr_child_requires_unsafe(&payload, "then")?
                        || self.expr_child_requires_unsafe(&payload, "else")?
                }
                "fold" => {
                    self.expr_child_requires_unsafe(&payload, "target")?
                        || self.expr_child_requires_unsafe(&payload, "init")?
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
    ) -> Result<String> {
        self.verify_expr_type_with_locals(
            expr_hash,
            root,
            param_types,
            allowed_regions,
            &mut Vec::new(),
        )
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
    ) -> Result<String> {
        let is_i64 = scrutinee_type == type_hash_for("I64");
        let mut result_type: Option<String> = None;
        let mut has_default = false;
        let mut seen_i64: BTreeSet<String> = BTreeSet::new();
        let mut seen_bool: BTreeSet<bool> = BTreeSet::new();
        for (index, arm) in arms.iter().enumerate() {
            if arm.get("binding_name").is_some() {
                bail!("scalar case arm cannot bind a value");
            }
            if arm.get("default").and_then(JsonValue::as_bool) == Some(true) {
                if index + 1 != arms.len() {
                    bail!("default case arm must be last");
                }
                if has_default {
                    bail!("duplicate default case arm");
                }
                has_default = true;
            } else if is_i64 {
                let value = arm
                    .get("literal_i64")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("scalar i64 case arm must be an integer literal or `_`"))?;
                value
                    .parse::<i64>()
                    .with_context(|| format!("invalid i64 case pattern {value}"))?;
                if !seen_i64.insert(value.to_string()) {
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
            )?;
            match &result_type {
                Some(expected) if expected != &body_type => bail!("case arm type mismatch"),
                Some(_) => {}
                None => result_type = Some(body_type),
            }
        }
        let exhaustive = has_default
            || (!is_i64 && seen_bool.contains(&true) && seen_bool.contains(&false));
        if !exhaustive {
            bail!("case expression is not exhaustive: a scalar `case` needs a `_` wildcard");
        }
        let actual_type = result_type.ok_or_else(|| anyhow!("case expression has no arms"))?;
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
            "literal_i64" => type_hash_for("I64"),
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
                let (expected_params, return_type) = self.signature_parts(&callee.signature)?;
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
                )?;
                let right = self.verify_expr_type_with_locals(
                    right_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                )?;
                let bool_hash = type_hash_for("Bool");
                match op {
                    "+" | "-" | "*" | "/" => {
                        let i64_hash = type_hash_for("I64");
                        if left != i64_hash || right != i64_hash {
                            bail!("integer op requires i64 operands");
                        }
                        i64_hash
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        if left != right {
                            bail!("comparison operands differ");
                        }
                        if left != type_hash_for("I64") && left != type_hash_for("U8") {
                            bail!("comparison op requires i64 or u8 operands");
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
                )?;
                match op {
                    "-" => {
                        if child_type != type_hash_for("I64") {
                            bail!("integer unary op requires i64 operand");
                        }
                        type_hash_for("I64")
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
                )? != type_hash_for("I64")
                    || self.verify_expr_type_with_locals(
                        len,
                        root,
                        param_types,
                        allowed_regions,
                        locals,
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
                )?;
                let value_type = self.verify_expr_type_with_locals(
                    value,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
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
                )?;
                if cond_type != type_hash_for("Bool") {
                    bail!("if condition must be bool");
                }
                let then_type = self.verify_expr_type_with_locals(
                    then_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                )?;
                let else_type = self.verify_expr_type_with_locals(
                    else_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
                )?;
                if then_type != else_type {
                    bail!("if branches must have the same type");
                }
                then_type
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
                );
                locals.pop();
                locals.pop();
                let body_type = body_type?;
                if !self.type_assignable_in_root(root, &body_type, &acc_type)? {
                    bail!("fold body type mismatch");
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
                )?;
                let index_type = self.verify_expr_type_with_locals(
                    index_hash,
                    root,
                    param_types,
                    allowed_regions,
                    locals,
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
                    );
                }
                let TypeSpec::Enum(variants) = self.type_spec_in_root(root, &scrutinee_type)?
                else {
                    bail!("case scrutinee must be an enum or scalar (i64/bool)");
                };
                let mut seen = BTreeSet::new();
                let mut result_type = None;
                let mut has_default = false;
                for (index, arm) in arms.iter().enumerate() {
                    let is_default = arm.get("default").and_then(JsonValue::as_bool) == Some(true);
                    let binding = arm.get("binding_name").and_then(JsonValue::as_str);
                    let mut binding_was_pushed = false;
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
                        if binding.is_some() {
                            bail!("default case arm cannot bind a payload");
                        }
                        has_default = true;
                    } else {
                        let variant = arm
                            .get("variant")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing variant"))?;
                        validate_projection_identifier("enum variant", variant)?;
                        if !seen.insert(variant.to_string()) {
                            bail!("duplicate case arm {variant}");
                        }
                        let variant_type = variants
                            .iter()
                            .find(|candidate| candidate.name == variant)
                            .map(|candidate| candidate.type_hash.clone())
                            .ok_or_else(|| anyhow!("case arm uses unknown variant {variant}"))?;
                        if let Some(binding) = binding {
                            validate_projection_identifier("case binding", binding)?;
                            locals.push(variant_type.clone());
                            binding_was_pushed = true;
                        } else if variant_type != type_hash_for("Unit") {
                            bail!("case arm {variant} must bind its payload");
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
                    );
                    if binding_was_pushed {
                        locals.pop();
                    }
                    let body_type = body_type?;
                    if let Some(expected) = &result_type {
                        if expected != &body_type {
                            bail!("case arm type mismatch");
                        }
                    } else {
                        result_type = Some(body_type);
                    }
                }
                let expected_variants = variants
                    .iter()
                    .map(|variant| variant.name.clone())
                    .collect::<BTreeSet<_>>();
                if !has_default && seen != expected_variants {
                    bail!("case expression must cover every enum variant");
                }
                result_type.ok_or_else(|| anyhow!("case expression has no arms"))?
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
    } = actual
    else {
        return None;
    };
    let TypeSpec::Named {
        type_symbol: expected_symbol,
        region_args: expected_args,
    } = expected
    else {
        return Some(false);
    };
    Some(actual_symbol == expected_symbol && actual_args == expected_args)
}

fn named_actual_type_assignable_for_call(
    actual: &TypeSpec,
    expected: &TypeSpec,
    callee_regions: &BTreeSet<String>,
) -> Option<bool> {
    let TypeSpec::Named {
        type_symbol: actual_symbol,
        region_args: actual_args,
    } = actual
    else {
        return None;
    };
    let TypeSpec::Named {
        type_symbol: expected_symbol,
        region_args: expected_args,
    } = expected
    else {
        return Some(false);
    };
    if actual_symbol != expected_symbol || actual_args.len() != expected_args.len() {
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
                "I64" => Ok("i64".to_string()),
                "U8" => Ok("u8".to_string()),
                "Bool" => Ok("bool".to_string()),
                "Unit" => Ok("unit".to_string()),
                other => bail!("unknown builtin type kind {other}"),
            },
            TypeSpec::Named {
                type_symbol,
                region_args,
            } => {
                if region_args.is_empty() {
                    Ok(format!("type<{type_symbol}>"))
                } else {
                    Ok(format!("type<{type_symbol}<{}>>", region_args.join(", ")))
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
        ParsedTypeSpec::Named { name, .. } => out.push(name.clone()),
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
        ParsedTypeSpec::Named { name, region_args } => {
            let head = match resolve_clique_type_name(name, module, name_to_local) {
                Some(local) => format!("@type-peer:{}", colors[local]),
                None => name.clone(),
            };
            json!({ "k": "named", "name": head, "regions": region_args })
        }
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

fn type_hash_for_spec(spec: &ParsedTypeSpec) -> Result<String> {
    match spec {
        ParsedTypeSpec::Builtin(kind) => Ok(type_hash_for(kind)),
        ParsedTypeSpec::Named { name, .. } => {
            bail!("named type {name} requires root-aware resolution")
        }
        ParsedTypeSpec::Reference { region, .. } | ParsedTypeSpec::Slice { region, .. } => {
            bail!("region '{region} requires root-aware resolution")
        }
        ParsedTypeSpec::RawPointer { .. }
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
        } => {
            validate_region_args(region_args)?;
            json!({
                "type_kind": "Named",
                "type_symbol": type_symbol,
                "region_args": region_args,
            })
        }
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
        "I64" => Ok(TypeSpec::Builtin("I64".to_string())),
        "U8" => Ok(TypeSpec::Builtin("U8".to_string())),
        "Bool" => Ok(TypeSpec::Builtin("Bool".to_string())),
        "Unit" => Ok(TypeSpec::Builtin("Unit".to_string())),
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
            Ok(TypeSpec::Named {
                type_symbol,
                region_args,
            })
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
            TypeToken::Ident(value) if value == "i64" || value == "I64" => {
                Ok(ParsedTypeSpec::Builtin("I64".to_string()))
            }
            TypeToken::Ident(value) if value == "u8" || value == "U8" => {
                Ok(ParsedTypeSpec::Builtin("U8".to_string()))
            }
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
                let region_args = self.parse_optional_region_args()?;
                Ok(ParsedTypeSpec::Named { name, region_args })
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

    fn parse_optional_region_args(&mut self) -> Result<Vec<String>> {
        if !self.consume_symbol("<") {
            return Ok(Vec::new());
        }
        let mut args = Vec::new();
        if self.consume_symbol(">") {
            bail!("region argument list must not be empty");
        }
        loop {
            self.expect_symbol("'")?;
            let name = self.expect_ident()?;
            validate_region_name("region argument", &name)?;
            args.push(name);
            if self.consume_symbol(">") {
                break;
            }
            self.expect_symbol(",")?;
        }
        Ok(args)
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
