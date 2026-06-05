use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::backend::ArtifactKind;
use crate::expr::RawExpr;
use crate::model::{
    ProgramRootPayload, TypeCheckResult, resolve_function_name_in_root, resolve_named_type_in_root,
    validate_projection_identifier,
};
use crate::store::{CodeDb, canonical_json, hash_object_canonical};
use crate::{ABI_TAG, DEFAULT_NATIVE_TARGET, MAIN_BRANCH, SCHEMA_VERSION};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveLoan {
    kind: LoanKind,
    region: String,
    place: LoanPlace,
    owner: Option<LoanPlace>,
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
    Value,
    Place,
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
        for type_name in ["I64", "Bool", "Unit"] {
            self.put_object("Type", &json!({ "type_kind": type_name }))?;
        }
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
            same_named_type_assignable(&self.type_spec(actual)?, &self.type_spec(expected)?)
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
            (TypeSpec::Record(actual_fields), TypeSpec::Record(expected_fields)) => {
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
        if let Some(assignable) = same_named_type_assignable_for_call(
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
            )
            | (
                TypeSpec::FixedArray {
                    element: actual, ..
                },
                TypeSpec::FixedArray {
                    element: expected, ..
                },
            ) => self.infer_call_region_substitutions(
                root,
                &actual,
                &expected,
                callee_regions,
                substitutions,
            ),
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
        self.validate_external_signature_effects(signature)?;
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

    fn validate_external_signature_effects(&self, signature: &str) -> Result<()> {
        let effects = self.signature_effects(signature)?;
        if !effects.contains(&Effect::Ffi) {
            bail!("external functions must declare the ffi effect");
        }
        Ok(())
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
        self.type_expr_with_locals(
            current_module,
            expr,
            root,
            param_names,
            param_types,
            region_scope,
            &mut Vec::new(),
        )
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
                    let typed = self.type_expr_with_locals(
                        current_module,
                        arg,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
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
                let i64_hash = type_hash_for("I64");
                let bool_hash = type_hash_for("Bool");
                let result_type = match op.as_str() {
                    "+" | "-" | "*" | "/" => {
                        require_type(&left.type_hash, &i64_hash, "left operand", self)?;
                        require_type(&right.type_hash, &i64_hash, "right operand", self)?;
                        i64_hash
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        require_type(&left.type_hash, &i64_hash, "left operand", self)?;
                        require_type(&right.type_hash, &i64_hash, "right operand", self)?;
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
                let type_hash = self.put_structural_type(TypeSpec::Reference {
                    region: region_hash.clone(),
                    mutable: false,
                    referent: target.type_hash.clone(),
                })?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "borrow_shared",
                        "target": target.expr_hash,
                        "region": region_hash,
                        "region_name": region_name,
                        "referent_type": target.type_hash,
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
                if !self.typed_expr_is_place(&target.expr_hash)? {
                    bail!("mutable borrow target must be an addressable place");
                }
                let (region_name, region_hash) = match region {
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
                let type_hash = self.put_structural_type(TypeSpec::Reference {
                    region: region_hash.clone(),
                    mutable: true,
                    referent: target.type_hash.clone(),
                })?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "borrow_mut",
                        "target": target.expr_hash,
                        "region": region_hash,
                        "region_name": region_name,
                        "referent_type": target.type_hash,
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
                let value = self.type_expr_with_locals(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                if !self.type_assignable_in_root(root, &value.type_hash, &binding_type)? {
                    require_type(&value.type_hash, &binding_type, "let binding", self)?;
                }
                locals.push(LocalTypeBinding {
                    name: name.clone(),
                    type_hash: binding_type.clone(),
                });
                let body = self.type_expr_with_locals(
                    current_module,
                    body,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
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
                let typed_value = self.type_expr_with_locals(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    region_scope,
                    locals,
                )?;
                require_type(
                    &typed_value.type_hash,
                    &variant_type,
                    "enum variant payload",
                    self,
                )?;
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
                let TypeSpec::Enum(variants) =
                    self.type_spec_in_root(root, &scrutinee.type_hash)?
                else {
                    bail!(
                        "case expression requires enum type, got {}",
                        self.type_name(&scrutinee.type_hash)?
                    );
                };
                if arms.is_empty() {
                    bail!("case expression must have at least one arm");
                }
                let mut seen = BTreeSet::new();
                let mut result_type: Option<String> = None;
                let mut arms_json = Vec::with_capacity(arms.len());
                for arm in arms {
                    validate_projection_identifier("enum variant", &arm.variant)?;
                    if !seen.insert(arm.variant.clone()) {
                        bail!("duplicate case arm {}", arm.variant);
                    }
                    let variant_type = variants
                        .iter()
                        .find(|variant| variant.name == arm.variant)
                        .map(|variant| variant.type_hash.clone())
                        .ok_or_else(|| anyhow!("case arm uses unknown variant {}", arm.variant))?;
                    if let Some(binding) = &arm.binding {
                        validate_projection_identifier("case binding", binding)?;
                        locals.push(LocalTypeBinding {
                            name: binding.clone(),
                            type_hash: variant_type.clone(),
                        });
                    } else if variant_type != type_hash_for("Unit") {
                        bail!("case arm {} must bind its payload", arm.variant);
                    }
                    let body = self.type_expr_with_locals(
                        current_module,
                        &arm.body,
                        root,
                        param_names,
                        param_types,
                        region_scope,
                        locals,
                    );
                    if arm.binding.is_some() {
                        locals.pop();
                    }
                    let body = body?;
                    if let Some(expected) = &result_type {
                        if expected != &body.type_hash {
                            bail!(
                                "case arm {} returns {}, expected {}",
                                arm.variant,
                                self.type_name(&body.type_hash)?,
                                self.type_name(expected)?
                            );
                        }
                    } else {
                        result_type = Some(body.type_hash.clone());
                    }
                    arms_json.push(json!({
                        "variant": arm.variant,
                        "binding_name": arm.binding,
                        "body": body.expr_hash,
                    }));
                }
                let expected_variants = variants
                    .iter()
                    .map(|variant| variant.name.clone())
                    .collect::<BTreeSet<_>>();
                if seen != expected_variants {
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

    fn typed_expr_is_place(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        Ok(matches!(
            payload.get("expr_kind").and_then(JsonValue::as_str),
            Some("param_ref" | "local_ref" | "field_access")
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
                self.validate_external_signature_effects(&entry.signature)
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
            "literal_i64" | "literal_bool" | "literal_unit" | "param_ref" => Ok(false),
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
                self.borrow_target_is_local_storage(root, target)
            }
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
            "case" => {
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
                        locals_with_local_borrows.push(false);
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
                if return_regions.is_empty() {
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
                }
                Ok(false)
            }
            "binary" | "unary" | "enum_construct" => Ok(false),
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
                    Ok(false)
                } else {
                    self.borrow_target_is_local_storage(root, target)
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
        self.collect_reference_regions_in_type(root, type_hash, &mut regions)?;
        Ok(regions)
    }

    fn collect_reference_regions_in_type(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        regions: &mut BTreeSet<String>,
    ) -> Result<()> {
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Builtin(_) => Ok(()),
            TypeSpec::Named { .. } => Ok(()),
            TypeSpec::Reference {
                region, referent, ..
            } => {
                regions.insert(region);
                self.collect_reference_regions_in_type(root, &referent, regions)
            }
            TypeSpec::RawPointer { pointee, .. } => {
                self.collect_reference_regions_in_type(root, &pointee, regions)
            }
            TypeSpec::FixedArray { element, .. } => {
                self.collect_reference_regions_in_type(root, &element, regions)
            }
            TypeSpec::Record(fields) | TypeSpec::Enum(fields) => {
                for field in fields {
                    self.collect_reference_regions_in_type(root, &field.type_hash, regions)?;
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
                    if !allowed_regions.contains(&region) {
                        bail!("invalid region reference {region}");
                    }
                }
                Ok(())
            }
            TypeSpec::Reference {
                region, referent, ..
            } => {
                if !allowed_regions.contains(&region) {
                    bail!("invalid region reference {region}");
                }
                self.validate_type_hash_in_root(root, &referent, allowed_regions)
            }
            TypeSpec::RawPointer { pointee, .. } => {
                self.validate_type_hash_in_root(root, &pointee, allowed_regions)
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
            "literal_i64" | "literal_bool" | "literal_unit" => Ok(()),
            "param_ref" | "local_ref" => match expr_use {
                ExprUse::Place => {
                    let place = self.loan_place_for_expr(expr_hash, param_types, &state.locals)?;
                    self.check_place_not_moved(&place, state)
                }
                ExprUse::Value => self.verify_place_value_use(root, expr_hash, param_types, state),
            },
            "call" => {
                let (args, _, _) = self.call_region_context(root, &payload)?;
                let mut added_call_loans = Vec::new();
                let mut moved_call_owners = Vec::new();
                for arg in args {
                    let pre_arg_state = state.clone();
                    let transfer_owner = self.move_source_place_for_expr(
                        root,
                        &arg,
                        param_types,
                        &pre_arg_state.locals,
                    )?;
                    self.verify_expr_borrows(root, &arg, param_types, state, ExprUse::Value)?;
                    let arg_loans =
                        self.collect_value_loans(root, &arg, param_types, &pre_arg_state)?;
                    self.check_loans_point_to_live_storage(&arg_loans, state)?;
                    if let Some(owner) = transfer_owner {
                        state
                            .active
                            .retain(|loan| !loan_owner_overlaps(loan, &owner));
                        moved_call_owners.push(owner);
                    }
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
                self.check_loan_conflicts(&kind, &place, &state.active)?;
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
                self.check_loan_conflicts(&LoanKind::Mutable, &target_place, &state.active)?;
                self.verify_expr_borrows(root, value, param_types, state, ExprUse::Value)?;
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
                    state
                        .active
                        .retain(|loan| !loan_owner_overlaps(loan, &target_place));
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
                let transfer_owner = self.move_source_place_for_expr(
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
                if let Some(owner) = &transfer_owner {
                    state
                        .active
                        .retain(|loan| !loan_owner_overlaps(loan, owner));
                }
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
                merge_branch_state(state, then_state, else_state);
                Ok(())
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
                Ok(())
            }
            "field_access" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing target"))?;
                self.verify_expr_borrows(root, target, param_types, state, ExprUse::Place)?;
                let place = self.loan_place_for_expr(expr_hash, param_types, &state.locals)?;
                self.check_place_not_moved(&place, state)?;
                match expr_use {
                    ExprUse::Place => Ok(()),
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
                });
            }
            "call" => {
                let (args, return_type, _) = self.call_region_context(root, &payload)?;
                let return_regions = self.reference_regions_in_type(root, &return_type)?;
                if return_regions.is_empty() {
                    return Ok(out);
                }
                for arg in args {
                    out.extend(
                        self.collect_value_loans(root, &arg, param_types, state)?
                            .into_iter()
                            .filter(|loan| return_regions.contains(&loan.region)),
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
                let transfer_owner = self.move_source_place_for_expr(
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
                if let Some(owner) = &transfer_owner {
                    nested_state
                        .active
                        .retain(|loan| !loan_owner_overlaps(loan, owner));
                }
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
            "param_ref" | "local_ref" | "field_access" => {
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
                let transfer_owner = self.move_source_place_for_expr(
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
                if let Some(owner) = &transfer_owner {
                    nested_state
                        .active
                        .retain(|loan| !loan_owner_overlaps(loan, owner));
                }
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
            _ => {
                let mut loans = self.collect_value_loans(root, expr_hash, param_types, state)?;
                let source_owner =
                    self.source_place_for_value_expr(expr_hash, param_types, &state.locals)?;
                for loan in &mut loans {
                    loan.owner = Some(rebased_loan_owner(
                        loan.owner.as_ref(),
                        source_owner.as_ref(),
                        target_owner,
                    ));
                }
                Ok(loans)
            }
        }
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
        self.check_shared_read_conflicts(&place, &state.active)?;
        let type_hash = self.expr_declared_type(expr_hash)?;
        let class = self.value_class_in_root(root, &type_hash)?;
        if class.copy_kind == ValueCopyKind::MoveOnly {
            state.moved.push(place);
        }
        Ok(())
    }

    fn move_source_place_for_expr(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        locals: &[usize],
    ) -> Result<Option<LoanPlace>> {
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("param_ref" | "local_ref" | "field_access") => {
                let type_hash = self.expr_declared_type(expr_hash)?;
                let class = self.value_class_in_root(root, &type_hash)?;
                if class.copy_kind == ValueCopyKind::MoveOnly {
                    Ok(Some(self.loan_place_for_expr(
                        expr_hash,
                        param_types,
                        locals,
                    )?))
                } else {
                    Ok(None)
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
                let source =
                    self.move_source_place_for_expr(root, body_hash, param_types, &nested_locals)?;
                let Some(source) = source else {
                    return Ok(None);
                };
                if matches!(source.root, LoanRoot::Local(id) if id == synthetic_local) {
                    Ok(None)
                } else {
                    Ok(Some(source))
                }
            }
            _ => Ok(None),
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
            Some("param_ref" | "local_ref" | "field_access") => Ok(Some(
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
            self.check_loan_conflicts(&loan.kind, &loan.place, active)?;
            if active.contains(loan) {
                continue;
            }
            active.push(loan.clone());
            added.push(loan.clone());
        }
        Ok(added)
    }

    fn check_loan_conflicts(
        &self,
        kind: &LoanKind,
        place: &LoanPlace,
        active: &[ActiveLoan],
    ) -> Result<()> {
        for loan in active {
            if places_overlap(place, &loan.place)
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
            if loan.kind == LoanKind::Mutable && places_overlap(place, &loan.place) {
                bail!(
                    "shared read of {:?} conflicts with live mutable borrow of {:?}",
                    place,
                    loan.place
                );
            }
        }
        Ok(())
    }

    fn expr_requires_state(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        Ok(
            match payload
                .get("expr_kind")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
            {
                "literal_i64" | "literal_bool" | "literal_unit" | "param_ref" | "local_ref" => {
                    false
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
                "let" => {
                    self.expr_child_requires_state(&payload, "value")?
                        || self.expr_child_requires_state(&payload, "body")?
                }
                "if" => {
                    self.expr_child_requires_state(&payload, "cond")?
                        || self.expr_child_requires_state(&payload, "then")?
                        || self.expr_child_requires_state(&payload, "else")?
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
                let i64_hash = type_hash_for("I64");
                let bool_hash = type_hash_for("Bool");
                match op {
                    "+" | "-" | "*" | "/" => {
                        if left != i64_hash || right != i64_hash {
                            bail!("integer op requires i64 operands");
                        }
                        i64_hash
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        if left != i64_hash || right != i64_hash {
                            bail!("comparison op requires i64 operands");
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
                if !allowed_regions.contains(region) {
                    bail!("invalid region reference {region}");
                }
                let referent_type = payload
                    .get("referent_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_shared missing referent_type"))?;
                if referent_type != target_type {
                    bail!("borrow_shared referent type mismatch");
                }
                hash_for_type_spec(&TypeSpec::Reference {
                    region: region.to_string(),
                    mutable: false,
                    referent: target_type,
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
                if !allowed_regions.contains(region) {
                    bail!("invalid region reference {region}");
                }
                let referent_type = payload
                    .get("referent_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("borrow_mut missing referent_type"))?;
                if referent_type != target_type {
                    bail!("borrow_mut referent type mismatch");
                }
                if !self.typed_expr_is_place(target)? {
                    bail!("borrow_mut target must be an addressable place");
                }
                hash_for_type_spec(&TypeSpec::Reference {
                    region: region.to_string(),
                    mutable: true,
                    referent: target_type,
                })?
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
                if value_type != variant_type {
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
                let TypeSpec::Enum(variants) = self.type_spec_in_root(root, &scrutinee_type)?
                else {
                    bail!("case scrutinee must be enum");
                };
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?;
                let mut seen = BTreeSet::new();
                let mut result_type = None;
                for arm in arms {
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
                    let binding = arm.get("binding_name").and_then(JsonValue::as_str);
                    if let Some(binding) = binding {
                        validate_projection_identifier("case binding", binding)?;
                        locals.push(variant_type.clone());
                    } else if variant_type != type_hash_for("Unit") {
                        bail!("case arm {variant} must bind its payload");
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
                    if binding.is_some() {
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
                if seen != expected_variants {
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

fn fields_prefix(prefix: &[String], value: &[String]) -> bool {
    prefix.len() <= value.len()
        && prefix
            .iter()
            .zip(value.iter())
            .all(|(left, right)| left == right)
}

fn same_named_type_assignable(actual: &TypeSpec, expected: &TypeSpec) -> Option<bool> {
    let (
        TypeSpec::Named {
            type_symbol: actual_symbol,
            region_args: actual_args,
        },
        TypeSpec::Named {
            type_symbol: expected_symbol,
            region_args: expected_args,
        },
    ) = (actual, expected)
    else {
        return None;
    };
    Some(actual_symbol == expected_symbol && actual_args == expected_args)
}

fn same_named_type_assignable_for_call(
    actual: &TypeSpec,
    expected: &TypeSpec,
    callee_regions: &BTreeSet<String>,
) -> Option<bool> {
    let (
        TypeSpec::Named {
            type_symbol: actual_symbol,
            region_args: actual_args,
        },
        TypeSpec::Named {
            type_symbol: expected_symbol,
            region_args: expected_args,
        },
    ) = (actual, expected)
    else {
        return None;
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
        ParsedTypeSpec::Reference { region, .. } => {
            bail!("reference region '{region} requires root-aware resolution")
        }
        ParsedTypeSpec::RawPointer { .. }
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
            TypeToken::Ident(value) if value == "bool" || value == "Bool" => {
                Ok(ParsedTypeSpec::Builtin("Bool".to_string()))
            }
            TypeToken::Ident(value) if value == "unit" || value == "Unit" => {
                Ok(ParsedTypeSpec::Builtin("Unit".to_string()))
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
        validate_projection_identifier("reference region", &region)?;
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
            validate_projection_identifier("region argument", &name)?;
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
