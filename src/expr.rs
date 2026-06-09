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
    Fold {
        item: String,
        target: Box<RawExpr>,
        acc: String,
        init: Box<RawExpr>,
        body: Box<RawExpr>,
    },
    Array {
        elements: Vec<RawExpr>,
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
    /// scalar scrutinee (R14). Mutually exclusive with `variant`/`default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<Box<RawExpr>>,
    #[serde(default, skip_serializing_if = "raw_case_arm_default_is_false")]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    pub body: RawExpr,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSource {
    pub module: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub region_params: Vec<String>,
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
    pub definition: TypeDefinitionKind,
    pub(crate) identity: Option<TypeDefinitionIdentity>,
}

#[derive(Debug, Clone)]
pub enum Value {
    I64(i64),
    U8(u8),
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

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::I64(left), Value::I64(right)) => left == right,
            (Value::U8(left), Value::U8(right)) => left == right,
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
            Value::I64(value) => write!(f, "{value}"),
            Value::U8(value) => write!(f, "{value}"),
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
        self.eval_expr(root_hash, &body, &mut args)
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
            (Value::I64(_), TypeSpec::Builtin(kind)) => Ok(kind == "I64"),
            (Value::U8(_), TypeSpec::Builtin(kind)) => Ok(kind == "U8"),
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
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .parse::<i64>()?;
                Ok(Value::I64(value))
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
                        let arm = arms
                            .iter()
                            .find(|arm| {
                                arm.get("variant").and_then(JsonValue::as_str) == Some(&variant)
                            })
                            .or_else(|| arms.iter().find(|arm| typed_case_arm_is_default(arm)))
                            .ok_or_else(|| anyhow!("case missing arm for variant {variant}"))?;
                        let body_hash = arm
                            .get("body")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing body"))?;
                        if arm
                            .get("binding_name")
                            .and_then(JsonValue::as_str)
                            .is_some()
                        {
                            locals.push(value);
                            let result =
                                self.eval_expr_with_locals(root_hash, body_hash, args, locals);
                            locals.pop();
                            result
                        } else {
                            self.eval_expr_with_locals(root_hash, body_hash, args, locals)
                        }
                    }
                    // Scalar literal `case` (R14): match the scalar value against
                    // literal-pattern arms, falling back to the `_` wildcard.
                    Value::I64(scrutinee) => {
                        let arm = arms
                            .iter()
                            .find(|arm| {
                                arm.get("literal_i64")
                                    .and_then(JsonValue::as_str)
                                    .and_then(|literal| literal.parse::<i64>().ok())
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
        let type_identity = TypeDefinitionIdentity {
            type_symbol_birth: self.symbol_birth_spec(definition.type_symbol())?,
            region_param_births: definition
                .region_params()
                .iter()
                .map(|param| self.symbol_birth_spec(&param.region))
                .collect::<Result<Vec<_>>>()?,
            member_births: Vec::new(),
        };
        let type_identity = canonical_json(&serde_json::to_value(&type_identity)?);
        let region_names = definition
            .region_params()
            .iter()
            .map(|param| (param.region.clone(), param.name.clone()))
            .collect::<BTreeMap<_, _>>();
        let region_suffix = if definition.region_params().is_empty() {
            String::new()
        } else {
            format!(
                "<{}>",
                definition
                    .region_params()
                    .iter()
                    .map(|param| format!("'{}", param.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        match definition {
            TypeDefinition::Record { fields, .. } => {
                let rendered_fields = fields
                    .iter()
                    .map(|field| {
                        let member_identity = canonical_json(&serde_json::to_value(
                            self.symbol_birth_spec(&field.member_symbol)?,
                        )?);
                        Ok(format!(
                            "  // codedb:member_identity {}\n  {}: {}",
                            member_identity,
                            field.name,
                            self.type_name_in_root_with_regions(
                                root,
                                &binding.module,
                                &field.type_hash,
                                &region_names,
                            )?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!(
                    "// codedb:type_identity {}\nrecord {}{} {{\n{}\n}}",
                    type_identity,
                    binding.display_name,
                    region_suffix,
                    rendered_fields.join("\n")
                ))
            }
            TypeDefinition::Enum { variants, .. } => {
                let rendered_variants = variants
                    .iter()
                    .map(|variant| {
                        let member_identity = canonical_json(&serde_json::to_value(
                            self.symbol_birth_spec(&variant.member_symbol)?,
                        )?);
                        Ok(format!(
                            "  // codedb:member_identity {}\n  {}: {}",
                            member_identity,
                            variant.name,
                            self.type_name_in_root_with_regions(
                                root,
                                &binding.module,
                                &variant.type_hash,
                                &region_names,
                            )?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!(
                    "// codedb:type_identity {}\nenum {}{} {{\n{}\n}}",
                    type_identity,
                    binding.display_name,
                    region_suffix,
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
            for dependency in self.dependencies_for_definition(root, &entry.definition)? {
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
                    self.type_name_in_root_with_regions(root, current_module, ty, &region_names)?
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut source = format!(
            "{}({}) -> {}",
            signature_region_suffix(&region_params),
            rendered_params.join(", "),
            self.type_name_in_root_with_regions(
                root,
                current_module,
                &return_type,
                &region_names,
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
                        self.type_name_in_root_with_regions(
                            root,
                            current_module,
                            enum_type,
                            region_names,
                        )?
                    )
                } else {
                    format!(
                        "{}::{variant}({})",
                        self.type_name_in_root_with_regions(
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
                    .enumerate()
                    .map(|(arm_index, arm)| {
                        // A nested low-precedence body — notably another `case`,
                        // whose `| arm` list the OUTER case would otherwise capture
                        // — must be parenthesized, except in the last arm where
                        // nothing follows it to capture. Without this, a nested
                        // non-last `case` projects to text that won't re-parse
                        // (SPEC_V3 §11 checked-view round-trip).
                        let body_prec = if arm_index + 1 == arm_count { 0 } else { 1 };
                        let binding = arm.get("binding_name").and_then(JsonValue::as_str);
                        if typed_case_arm_is_default(arm) {
                            if binding.is_some() {
                                bail!("default case arm cannot bind a payload");
                            }
                            let body = arm
                                .get("body")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("case arm missing body"))?;
                            return Ok(format!(
                                "else => {}",
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
                                "{pattern} => {}",
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
                        if let Some(binding) = binding {
                            local_names.push(binding.to_string());
                        }
                        let rendered_body = self.expr_to_source_with_locals(
                            body,
                            root,
                            current_module,
                            local_params,
                            region_names,
                            local_names,
                            body_prec,
                        );
                        if binding.is_some() {
                            local_names.pop();
                        }
                        Ok(match binding {
                            Some(binding) => {
                                format!("{variant}({binding}) => {}", rendered_body?)
                            }
                            None => format!("{variant} => {}", rendered_body?),
                        })
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
        )
    }

    fn typed_expr_to_raw_with_locals(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        region_names: &BTreeMap<String, String>,
        local_names: &mut Vec<String>,
    ) -> Result<RawExpr> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => Ok(RawExpr::LiteralI64 {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .to_string(),
            }),
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
                    )?,
                ],
            }),
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
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
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
                    )?,
                ),
                field: payload
                    .get("field")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing field"))?
                    .to_string(),
            }),
            "enum_construct" => Ok(RawExpr::EnumConstruct {
                enum_type: self.type_name_in_root_with_regions(
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
                            )?;
                            return Ok(RawCaseArm {
                                variant: None,
                                literal: None,
                                default: true,
                                binding: None,
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
                            )?;
                            return Ok(RawCaseArm {
                                variant: None,
                                literal: Some(literal),
                                default: false,
                                binding: None,
                                body,
                            });
                        }
                        let variant = arm
                            .get("variant")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing variant"))?
                            .to_string();
                        if let Some(binding) = &binding {
                            local_names.push(binding.clone());
                        }
                        let body = self.typed_expr_to_raw_with_locals(
                            arm.get("body")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("case arm missing body"))?,
                            root,
                            current_module,
                            region_names,
                            local_names,
                        );
                        if binding.is_some() {
                            local_names.pop();
                        }
                        Ok(RawCaseArm {
                            variant: Some(variant),
                            literal: None,
                            default: false,
                            binding,
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
    8
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
            TypeSpec::Named { type_symbol, .. } => {
                deps.insert(type_symbol);
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
                if self.root_symbol(root, symbol).is_some() {
                    deps.insert(symbol.to_string());
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
            "fold" => {
                for key in ["target", "init", "body"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("fold missing {key}"))?;
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
}

impl Parser {
    fn new(source: &str) -> Result<Self> {
        Ok(Self {
            tokens: lex(source)?,
            pos: 0,
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
        let region_params = self.parse_optional_region_params()?;
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
            definition,
            identity,
        })
    }

    fn parse_function_in_module(&mut self, module: String) -> Result<FunctionSource> {
        self.expect_ident_value("fn")?;
        let name = self.expect_ident()?;
        let region_params = self.parse_optional_region_params()?;
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
                    // Wildcard / default arm (R14 `_`, or the `else`/`default` keyword).
                    self.expect_symbol("=>")?;
                    let body = self.parse_expr()?;
                    arms.push(RawCaseArm {
                        variant: None,
                        literal: None,
                        default: true,
                        binding: None,
                        body,
                    });
                    if self.consume_symbol("|") {
                        bail!("default case arm must be last");
                    }
                    break;
                } else if let Some(literal) = self.try_parse_scalar_literal_pattern()? {
                    // Scalar literal pattern (R14): `0 => ...`, `true => ...`.
                    self.expect_symbol("=>")?;
                    let body = self.parse_expr()?;
                    arms.push(RawCaseArm {
                        variant: None,
                        literal: Some(Box::new(literal)),
                        default: false,
                        binding: None,
                        body,
                    });
                    if !self.consume_symbol("|") {
                        break;
                    }
                } else {
                    let variant = self.expect_ident()?;
                    let binding = if self.consume_symbol("(") {
                        let binding = self.expect_ident()?;
                        self.expect_symbol(")")?;
                        Some(binding)
                    } else {
                        None
                    };
                    self.expect_symbol("=>")?;
                    let body = self.parse_expr()?;
                    arms.push(RawCaseArm {
                        variant: Some(variant),
                        literal: None,
                        default: false,
                        binding,
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
            let op = match self.peek() {
                Token::Symbol(op) if is_binary_op(op) => op.clone(),
                _ => break,
            };
            let prec = op_precedence(&op);
            if prec < min_prec {
                break;
            }
            self.next();
            let right = self.parse_binary_prec(prec + 1)?;
            left = RawExpr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<RawExpr> {
        match self.peek() {
            Token::Symbol(op) if op == "-" || op == "!" => {
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
                let mut elements = Vec::new();
                if self.consume_symbol("]") {
                    bail!("array literal must have at least one element");
                }
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
        match name.as_str() {
            "i64" | "I64" => Ok("i64".to_string()),
            "u8" | "U8" => Ok("u8".to_string()),
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
                let region_args = self.parse_optional_type_region_args()?;
                if region_args.is_empty() {
                    Ok(path)
                } else {
                    Ok(format!(
                        "{}<{}>",
                        path,
                        region_args
                            .into_iter()
                            .map(|name| format!("'{name}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
        }
    }

    fn parse_optional_type_region_args(&mut self) -> Result<Vec<String>> {
        if !self.consume_symbol_raw("<") {
            return Ok(Vec::new());
        }
        let mut args = Vec::new();
        if self.consume_symbol_raw(">") {
            bail!("region argument list must not be empty");
        }
        loop {
            self.expect_symbol("'")?;
            args.push(self.expect_ident()?);
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
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
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
                "->" | "==" | "!=" | "<=" | ">=" | "&&" | "||" | "::" | "=>"
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
    matches!(
        op,
        "+" | "-" | "*" | "/" | "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||"
    )
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

fn semantic_clone_value(value: &Value) -> Value {
    match value {
        Value::I64(value) => Value::I64(*value),
        Value::U8(value) => Value::U8(*value),
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
