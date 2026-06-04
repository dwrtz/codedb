use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::backend::ArtifactKind;
use crate::expr::RawExpr;
use crate::model::{
    ProgramRootPayload, TypeCheckResult, resolve_function_name_in_root,
    validate_projection_identifier,
};
use crate::store::{CodeDb, canonical_json, hash_object_canonical};
use crate::{ABI_TAG, MAIN_BRANCH, SCHEMA_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
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
pub(crate) struct TypeFieldSpec {
    pub(crate) name: String,
    pub(crate) type_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypeSpec {
    Builtin(String),
    Record(Vec<TypeFieldSpec>),
    Enum(Vec<TypeFieldSpec>),
}

impl CodeDb {
    pub(crate) fn insert_builtin_types(&mut self) -> Result<()> {
        for type_name in ["I64", "Bool", "Unit"] {
            self.put_object("Type", &json!({ "type_kind": type_name }))?;
        }
        Ok(())
    }

    pub(crate) fn resolve_type(&mut self, ty: &str) -> Result<String> {
        let parsed = parse_type_source(ty)?;
        self.put_type_spec(&parsed)
    }

    pub(crate) fn type_hash_for_source(&self, ty: &str) -> Result<String> {
        let parsed = parse_type_source(ty)?;
        Ok(type_hash_for_spec(&parsed))
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

    pub(crate) fn record_field_type(&self, type_hash: &str, field: &str) -> Result<String> {
        match self.type_spec(type_hash)? {
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

    pub(crate) fn enum_variant_type(&self, type_hash: &str, variant: &str) -> Result<String> {
        match self.type_spec(type_hash)? {
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

    fn put_type_spec(&mut self, spec: &ParsedTypeSpec) -> Result<String> {
        match spec {
            ParsedTypeSpec::Builtin(kind) => Ok(type_hash_for(kind)),
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
        let effects = normalize_effects(effects)?;
        self.put_object(
            "FunctionSignature",
            &json!({
                "params": param_types,
                "return": return_type,
                "abi": ABI_TAG,
                "effects": effect_names(&effects),
            }),
        )
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

    pub(crate) fn put_symbol_birth(
        &mut self,
        parent_history_hash: Option<&str>,
        birth_seed: &str,
    ) -> Result<String> {
        self.put_object(
            "SymbolBirth",
            &json!({
                "symbol_kind": "function",
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

    pub(crate) fn function_body_hash(&self, definition_hash: &str) -> Result<String> {
        let payload = self.get_payload(definition_hash)?;
        payload
            .get("typed_body_expr_hash")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("function definition missing typed_body_expr_hash"))
    }

    pub(crate) fn function_signature_hash(&self, definition_hash: &str) -> Result<String> {
        let payload = self.get_payload(definition_hash)?;
        payload
            .get("function_sig_hash")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("function definition missing function_sig_hash"))
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
        self.type_expr_with_locals(
            current_module,
            expr,
            root,
            param_names,
            param_types,
            &mut Vec::new(),
        )
    }

    fn type_expr_with_locals(
        &mut self,
        current_module: &str,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
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
                        locals,
                    )?;
                    for field in fields.split('.') {
                        typed = self.type_field_access(&typed, field)?;
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
                for (idx, arg) in args.iter().enumerate() {
                    let typed = self.type_expr_with_locals(
                        current_module,
                        arg,
                        root,
                        param_names,
                        param_types,
                        locals,
                    )?;
                    if typed.type_hash != expected_params[idx] {
                        bail!(
                            "call arg {} for {name} expected {}, got {}",
                            idx,
                            self.type_name(&expected_params[idx])?,
                            self.type_name(&typed.type_hash)?
                        );
                    }
                    typed_args.push(typed.expr_hash);
                }
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
                    locals,
                )?;
                let right = self.type_expr_with_locals(
                    current_module,
                    right,
                    root,
                    param_names,
                    param_types,
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
            RawExpr::Let {
                name,
                ty,
                value,
                body,
            } => {
                validate_projection_identifier("let binding", name)?;
                let binding_type = self.resolve_type(ty)?;
                let value = self.type_expr_with_locals(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
                    locals,
                )?;
                require_type(&value.type_hash, &binding_type, "let binding", self)?;
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
                    locals,
                )?;
                let else_expr = self.type_expr_with_locals(
                    current_module,
                    else_expr,
                    root,
                    param_names,
                    param_types,
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
                    locals,
                )?;
                self.type_field_access(&target, field)
            }
            RawExpr::EnumConstruct {
                enum_type,
                variant,
                value,
            } => {
                validate_projection_identifier("enum variant", variant)?;
                let enum_type_hash = self.resolve_type(enum_type)?;
                let variant_type = self.enum_variant_type(&enum_type_hash, variant)?;
                let typed_value = self.type_expr_with_locals(
                    current_module,
                    value,
                    root,
                    param_names,
                    param_types,
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
                    locals,
                )?;
                let TypeSpec::Enum(variants) = self.type_spec(&scrutinee.type_hash)? else {
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
        target: &TypeCheckResult,
        field: &str,
    ) -> Result<TypeCheckResult> {
        let field_type = self.record_field_type(&target.type_hash, field)?;
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

    pub(crate) fn type_check_root(&self, root_hash: &str) -> Result<()> {
        let root = self.load_root(root_hash)?;
        for entry in &root.symbols {
            let (param_types, return_type) = self.signature_parts(&entry.signature)?;
            self.signature_effects(&entry.signature)?;
            let body = self.function_body_hash(&entry.definition)?;
            let actual = self.verify_expr_type(&body, &root, &param_types)?;
            if actual != return_type {
                bail!(
                    "bad_type: function {} returns {}, body is {}",
                    self.symbol_display(&root, &entry.symbol)?,
                    self.type_name(&return_type)?,
                    self.type_name(&actual)?
                );
            }
            let definition_signature = self.function_signature_hash(&entry.definition)?;
            if definition_signature != entry.signature {
                bail!(
                    "bad_signature: root signature {} does not match definition signature {}",
                    entry.signature,
                    definition_signature
                );
            }
            self.verify_function_effects(&root, entry)?;
        }
        self.validate_tests_for_root(root_hash, &root)?;
        Ok(())
    }

    fn verify_function_effects(
        &self,
        root: &ProgramRootPayload,
        entry: &crate::model::RootSymbolPayload,
    ) -> Result<()> {
        let declared = self
            .signature_effects(&entry.signature)?
            .into_iter()
            .collect::<BTreeSet<_>>();
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

    pub(crate) fn verify_expr_type(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        param_types: &[String],
    ) -> Result<String> {
        self.verify_expr_type_with_locals(expr_hash, root, param_types, &mut Vec::new())
    }

    fn verify_expr_type_with_locals(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        param_types: &[String],
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
                for (idx, arg) in args.iter().enumerate() {
                    let arg_hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    let arg_type =
                        self.verify_expr_type_with_locals(arg_hash, root, param_types, locals)?;
                    if arg_type != expected_params[idx] {
                        bail!("call arg type mismatch for {symbol} at arg {idx}");
                    }
                }
                return_type
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
                let left =
                    self.verify_expr_type_with_locals(left_hash, root, param_types, locals)?;
                let right =
                    self.verify_expr_type_with_locals(right_hash, root, param_types, locals)?;
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
                let child_type =
                    self.verify_expr_type_with_locals(child, root, param_types, locals)?;
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
                let value_type =
                    self.verify_expr_type_with_locals(value_hash, root, param_types, locals)?;
                if value_type != binding_type {
                    bail!("let binding type mismatch");
                }
                locals.push(binding_type);
                let body_type =
                    self.verify_expr_type_with_locals(body_hash, root, param_types, locals);
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
                let cond_type =
                    self.verify_expr_type_with_locals(cond, root, param_types, locals)?;
                if cond_type != type_hash_for("Bool") {
                    bail!("if condition must be bool");
                }
                let then_type =
                    self.verify_expr_type_with_locals(then_hash, root, param_types, locals)?;
                let else_type =
                    self.verify_expr_type_with_locals(else_hash, root, param_types, locals)?;
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
                    let field_type =
                        self.verify_expr_type_with_locals(value_hash, root, param_types, locals)?;
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
                let target_type =
                    self.verify_expr_type_with_locals(target_hash, root, param_types, locals)?;
                self.record_field_type(&target_type, field)?
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
                let variant_type = self.enum_variant_type(enum_type, variant)?;
                let value_hash = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("enum_construct missing value"))?;
                let value_type =
                    self.verify_expr_type_with_locals(value_hash, root, param_types, locals)?;
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
                let scrutinee_type =
                    self.verify_expr_type_with_locals(scrutinee_hash, root, param_types, locals)?;
                let TypeSpec::Enum(variants) = self.type_spec(&scrutinee_type)? else {
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
                    let body_type =
                        self.verify_expr_type_with_locals(body_hash, root, param_types, locals);
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

impl TypeSpec {
    pub(crate) fn to_source(&self, db: &CodeDb) -> Result<String> {
        match self {
            TypeSpec::Builtin(kind) => match kind.as_str() {
                "I64" => Ok("i64".to_string()),
                "Bool" => Ok("bool".to_string()),
                "Unit" => Ok("unit".to_string()),
                other => bail!("unknown builtin type kind {other}"),
            },
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
    Record(Vec<ParsedTypeField>),
    Enum(Vec<ParsedTypeField>),
}

impl ParsedTypeSpec {
    fn to_payload_spec(&self) -> TypeSpec {
        match self {
            ParsedTypeSpec::Builtin(kind) => TypeSpec::Builtin(kind.clone()),
            ParsedTypeSpec::Record(fields) => TypeSpec::Record(
                fields
                    .iter()
                    .map(|field| TypeFieldSpec {
                        name: field.name.clone(),
                        type_hash: type_hash_for_spec(&field.ty),
                    })
                    .collect(),
            ),
            ParsedTypeSpec::Enum(variants) => TypeSpec::Enum(
                variants
                    .iter()
                    .map(|variant| TypeFieldSpec {
                        name: variant.name.clone(),
                        type_hash: type_hash_for_spec(&variant.ty),
                    })
                    .collect(),
            ),
        }
    }
}

fn parse_type_source(source: &str) -> Result<ParsedTypeSpec> {
    let mut parser = TypeParser::new(source)?;
    let spec = parser.parse_type()?;
    parser.expect_eof()?;
    Ok(spec)
}

fn type_hash_for_spec(spec: &ParsedTypeSpec) -> String {
    match spec {
        ParsedTypeSpec::Builtin(kind) => type_hash_for(kind),
        ParsedTypeSpec::Record(_) | ParsedTypeSpec::Enum(_) => {
            let payload = type_payload_for_spec(&spec.to_payload_spec())
                .expect("parsed type spec is already validated");
            hash_object_canonical("Type", SCHEMA_VERSION, &canonical_json(&payload))
        }
    }
}

pub(crate) fn type_payload_for_spec(spec: &TypeSpec) -> Result<JsonValue> {
    Ok(match spec {
        TypeSpec::Builtin(kind) => json!({ "type_kind": kind }),
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
            TypeToken::Symbol(value) if value == "(" => {
                self.expect_symbol(")")?;
                Ok(ParsedTypeSpec::Builtin("Unit".to_string()))
            }
            other => bail!("expected type, got {other:?}"),
        }
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
        } else {
            tokens.push(TypeToken::Symbol(ch.to_string()));
            i += 1;
        }
    }
    tokens.push(TypeToken::Eof);
    Ok(tokens)
}
