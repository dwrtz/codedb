use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::backend::ArtifactKind;
use crate::expr::RawExpr;
use crate::model::{
    ProgramRootPayload, TypeCheckResult, resolve_name_in_root, validate_projection_identifier,
};
use crate::store::{CodeDb, canonical_json, hash_object_canonical};
use crate::{ABI_TAG, SCHEMA_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
}

#[derive(Debug, Clone)]
struct LocalTypeBinding {
    name: String,
    type_hash: String,
}

impl CodeDb {
    pub(crate) fn insert_builtin_types(&mut self) -> Result<()> {
        for type_name in ["I64", "Bool", "Unit"] {
            self.put_object("Type", &json!({ "type_kind": type_name }))?;
        }
        Ok(())
    }

    pub(crate) fn resolve_type(&self, ty: &str) -> Result<String> {
        match ty {
            "i64" | "I64" => Ok(type_hash_for("I64")),
            "bool" | "Bool" => Ok(type_hash_for("Bool")),
            "unit" | "Unit" | "()" => Ok(type_hash_for("Unit")),
            other => bail!("unknown type {other}"),
        }
    }

    pub(crate) fn type_name(&self, hash: &str) -> Result<&'static str> {
        if hash == type_hash_for("I64") {
            Ok("i64")
        } else if hash == type_hash_for("Bool") {
            Ok("bool")
        } else if hash == type_hash_for("Unit") {
            Ok("unit")
        } else {
            bail!("unknown type hash {hash}")
        }
    }

    pub(crate) fn put_signature(
        &mut self,
        param_types: &[String],
        return_type: &str,
    ) -> Result<String> {
        self.put_object(
            "FunctionSignature",
            &json!({
                "params": param_types,
                "return": return_type,
                "abi": ABI_TAG,
                "effects": [],
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

    pub(crate) fn type_expr(
        &mut self,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
    ) -> Result<TypeCheckResult> {
        self.type_expr_with_locals(expr, root, param_names, param_types, &mut Vec::new())
    }

    fn type_expr_with_locals(
        &mut self,
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
                        &RawExpr::ParamRef { index },
                        root,
                        param_names,
                        param_types,
                        locals,
                    )
                }
            }
            RawExpr::Call { name, args } => {
                let symbol = resolve_name_in_root(root, "main", name)
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
                    let typed =
                        self.type_expr_with_locals(arg, root, param_names, param_types, locals)?;
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
                let left =
                    self.type_expr_with_locals(left, root, param_names, param_types, locals)?;
                let right =
                    self.type_expr_with_locals(right, root, param_names, param_types, locals)?;
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
                let typed =
                    self.type_expr_with_locals(expr, root, param_names, param_types, locals)?;
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
                let value =
                    self.type_expr_with_locals(value, root, param_names, param_types, locals)?;
                require_type(&value.type_hash, &binding_type, "let binding", self)?;
                locals.push(LocalTypeBinding {
                    name: name.clone(),
                    type_hash: binding_type.clone(),
                });
                let body = self.type_expr_with_locals(body, root, param_names, param_types, locals);
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
                let cond =
                    self.type_expr_with_locals(cond, root, param_names, param_types, locals)?;
                let bool_hash = type_hash_for("Bool");
                require_type(&cond.type_hash, &bool_hash, "if condition", self)?;
                let then_expr =
                    self.type_expr_with_locals(then_expr, root, param_names, param_types, locals)?;
                let else_expr =
                    self.type_expr_with_locals(else_expr, root, param_names, param_types, locals)?;
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
        }
    }

    pub(crate) fn type_check_root(&self, root_hash: &str) -> Result<()> {
        let root = self.load_root(root_hash)?;
        for entry in &root.symbols {
            let (param_types, return_type) = self.signature_parts(&entry.signature)?;
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
        }
        self.validate_tests_for_root(root_hash, &root)?;
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
