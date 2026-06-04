use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::MAIN_BRANCH;
use crate::model::{
    NameBinding, ProgramRootPayload, RootSymbolPayload, param_names, preferred_names,
    root_module_names,
};
use crate::store::CodeDb;
use crate::types::{Effect, ParamSpec, TypeSpec, normalize_effects, visible_effects};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RawExpr {
    LiteralI64 {
        value: String,
    },
    LiteralBool {
        value: bool,
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
    pub variant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    pub body: RawExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSource {
    pub module: String,
    pub name: String,
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
    pub params: Vec<ParamSpec>,
    pub return_type: String,
    pub effects: Vec<Effect>,
    pub abi: String,
    pub link_name: String,
    pub library: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramItem {
    Function(FunctionSource),
    ExternalFunction(ExternalFunctionSource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    I64(i64),
    Bool(bool),
    Unit,
    Record(BTreeMap<String, Value>),
    Enum { variant: String, value: Box<Value> },
}

impl Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::I64(value) => write!(f, "{value}"),
            Value::Bool(value) => write!(f, "{value}"),
            Value::Unit => write!(f, "()"),
            Value::Record(fields) => {
                let rendered = fields
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}"))
                    .collect::<Vec<_>>();
                write!(f, "{{{}}}", rendered.join(", "))
            }
            Value::Enum { variant, value } => {
                if matches!(value.as_ref(), Value::Unit) {
                    write!(f, "{variant}")
                } else {
                    write!(f, "{variant}({value})")
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
        let symbol = self.resolve_name(root_hash, "main", function_name)?;
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
            if !self.value_has_type(arg, ty)? {
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
        self.eval_expr(root_hash, &body, &args)
    }

    pub(crate) fn eval_expr(
        &self,
        root_hash: &str,
        expr_hash: &str,
        args: &[Value],
    ) -> Result<Value> {
        self.eval_expr_with_locals(root_hash, expr_hash, args, &mut Vec::new())
    }

    pub(crate) fn value_has_type(&self, value: &Value, type_hash: &str) -> Result<bool> {
        match (value, self.type_spec(type_hash)?) {
            (Value::I64(_), TypeSpec::Builtin(kind)) => Ok(kind == "I64"),
            (Value::Bool(_), TypeSpec::Builtin(kind)) => Ok(kind == "Bool"),
            (Value::Unit, TypeSpec::Builtin(kind)) => Ok(kind == "Unit"),
            (Value::Record(values), TypeSpec::Record(fields)) => {
                if values.len() != fields.len() {
                    return Ok(false);
                }
                for field in fields {
                    let Some(value) = values.get(&field.name) else {
                        return Ok(false);
                    };
                    if !self.value_has_type(value, &field.type_hash)? {
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
                self.value_has_type(value, &variant.type_hash)
            }
            _ => Ok(false),
        }
    }

    fn eval_expr_with_locals(
        &self,
        root_hash: &str,
        expr_hash: &str,
        args: &[Value],
        locals: &mut Vec<Value>,
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
            "literal_unit" => Ok(Value::Unit),
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                args.get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("parameter index out of bounds: {index}"))
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                local_at_depth(locals, depth)
                    .cloned()
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
                locals.push(value);
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
                        self.eval_expr_with_locals(root_hash, value_hash, args, locals)?,
                    );
                }
                Ok(Value::Record(values))
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
                match self.eval_expr_with_locals(root_hash, target_hash, args, locals)? {
                    Value::Record(fields) => fields
                        .get(field)
                        .cloned()
                        .ok_or_else(|| anyhow!("record value has no field {field}")),
                    other => bail!("field access target evaluated to non-record {other}"),
                }
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
                    value: Box::new(
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
                let Value::Enum { variant, value } = value else {
                    bail!("case expression evaluated to non-enum {value}");
                };
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?;
                let arm = arms
                    .iter()
                    .find(|arm| arm.get("variant").and_then(JsonValue::as_str) == Some(&variant))
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
                    locals.push(*value);
                    let result = self.eval_expr_with_locals(root_hash, body_hash, args, locals);
                    locals.pop();
                    result
                } else {
                    self.eval_expr_with_locals(root_hash, body_hash, args, locals)
                }
            }
            other => bail!("unknown expression kind {other}"),
        }
    }

    pub(crate) fn render_source(&self, root_hash: &str) -> Result<String> {
        let root = self.load_root(root_hash)?;
        let mut chunks = Vec::new();
        let has_non_main_modules = root_module_names(&root)
            .iter()
            .any(|name| name != MAIN_BRANCH);
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
                self.external_signature_source(
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
        Ok(format!(
            "fn {}{} = {}",
            binding.display_name,
            self.signature_source(&root_symbol.signature, &param_names(root, &binding.symbol),)?,
            self.expr_to_source_in_module(
                &body,
                root,
                &binding.module,
                &param_names(root, &binding.symbol),
                0,
            )?
        ))
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
                Ok(format!("{name}: {}", self.type_name(ty)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut source = format!(
            "({}) -> {}",
            rendered_params.join(", "),
            self.type_name(&return_type)?
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

    pub(crate) fn external_signature_source(
        &self,
        signature_hash: &str,
        param_names: &[String],
        abi: &str,
    ) -> Result<String> {
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
                Ok(format!("{name}: {}", self.type_name(ty)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut source = format!(
            "({}) -> {} abi[{abi}]",
            rendered_params.join(", "),
            self.type_name(&return_type)?
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
        self.expr_to_source_with_locals(
            expr_hash,
            root,
            current_module,
            local_params,
            &mut Vec::new(),
            parent_prec,
        )
    }

    fn expr_to_source_with_locals(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
        local_params: &[String],
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
                        local_names,
                        prec,
                    )?,
                    op,
                    self.expr_to_source_with_locals(
                        right,
                        root,
                        current_module,
                        local_params,
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
                    local_names,
                    0,
                )?;
                local_names.push(name.to_string());
                let body = self.expr_to_source_with_locals(
                    body_hash,
                    root,
                    current_module,
                    local_params,
                    local_names,
                    0,
                );
                local_names.pop();
                let expr = format!(
                    "let {name}: {} = {value} in {}",
                    self.type_name(binding_type)?,
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
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        then_hash,
                        root,
                        current_module,
                        local_params,
                        local_names,
                        0,
                    )?,
                    self.expr_to_source_with_locals(
                        else_hash,
                        root,
                        current_module,
                        local_params,
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
                                local_names,
                                0,
                            )?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                format!("{{{}}}", fields.join(", "))
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
                    format!("{}::{variant}", self.type_name(enum_type)?)
                } else {
                    format!(
                        "{}::{variant}({})",
                        self.type_name(enum_type)?,
                        self.expr_to_source_with_locals(
                            value,
                            root,
                            current_module,
                            local_params,
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
                let rendered_arms = arms
                    .iter()
                    .map(|arm| {
                        let variant = arm
                            .get("variant")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing variant"))?;
                        let binding = arm.get("binding_name").and_then(JsonValue::as_str);
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
                            local_names,
                            0,
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
        self.typed_expr_to_raw_with_locals(expr_hash, root, current_module, &mut Vec::new())
    }

    fn typed_expr_to_raw_with_locals(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        current_module: &str,
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
                    local_names,
                );
                local_names.pop();
                Ok(RawExpr::Let {
                    name,
                    ty: self.type_name(binding_type)?.to_string(),
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
                        local_names,
                    )?,
                ),
            }),
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
                            local_names,
                        )?;
                        Ok(RawRecordField { name, value })
                    })
                    .collect::<Result<Vec<_>>>()?,
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
                enum_type: self.type_name(
                    payload
                        .get("enum_type")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("enum_construct missing enum_type"))?,
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
                    local_names,
                )?;
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?
                    .iter()
                    .map(|arm| {
                        let variant = arm
                            .get("variant")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("case arm missing variant"))?
                            .to_string();
                        let binding = arm
                            .get("binding_name")
                            .and_then(JsonValue::as_str)
                            .map(str::to_string);
                        if let Some(binding) = &binding {
                            local_names.push(binding.clone());
                        }
                        let body = self.typed_expr_to_raw_with_locals(
                            arm.get("body")
                                .and_then(JsonValue::as_str)
                                .ok_or_else(|| anyhow!("case arm missing body"))?,
                            root,
                            current_module,
                            local_names,
                        );
                        if binding.is_some() {
                            local_names.pop();
                        }
                        Ok(RawCaseArm {
                            variant,
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
    match (op, left, right) {
        ("+", Value::I64(a), Value::I64(b)) => Ok(Value::I64(a + b)),
        ("-", Value::I64(a), Value::I64(b)) => Ok(Value::I64(a - b)),
        ("*", Value::I64(a), Value::I64(b)) => Ok(Value::I64(a * b)),
        ("/", Value::I64(_), Value::I64(0)) => bail!("division by zero"),
        ("/", Value::I64(a), Value::I64(b)) => Ok(Value::I64(a / b)),
        ("==", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a == b)),
        ("!=", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a != b)),
        ("<", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a < b)),
        ("<=", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a <= b)),
        (">", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a > b)),
        (">=", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a >= b)),
        ("&&", Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
        ("||", Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
        (op, left, right) => bail!("invalid operands for {op}: {left}, {right}"),
    }
}

pub(crate) fn eval_unary(op: &str, value: Value) -> Result<Value> {
    match (op, value) {
        ("-", Value::I64(value)) => Ok(Value::I64(-value)),
        ("!", Value::Bool(value)) => Ok(Value::Bool(!value)),
        (op, value) => bail!("invalid operand for {op}: {value}"),
    }
}

pub(crate) fn op_precedence(op: &str) -> u8 {
    match op {
        "||" => 1,
        "&&" => 2,
        "==" | "!=" => 3,
        "<" | "<=" | ">" | ">=" => 4,
        "+" | "-" => 5,
        "*" | "/" => 6,
        _ => 9,
    }
}

pub(crate) fn unary_precedence() -> u8 {
    7
}

fn field_access_precedence() -> u8 {
    8
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
            "literal_i64" | "literal_bool" | "literal_unit" | "param_ref" | "local_ref" => {}
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
    Symbol(String),
    Eof,
}

pub(crate) fn parse_program(source: &str) -> Result<Vec<ProgramItem>> {
    let mut parser = Parser::new(source)?;
    let mut items = Vec::new();
    while !parser.at_eof() {
        if parser.consume_ident_value("module") {
            let module = parser.expect_name_path()?;
            parser.expect_symbol("{")?;
            while !parser.consume_symbol("}") {
                if parser.at_eof() {
                    bail!("unterminated module {module}");
                }
                items.push(parser.parse_program_item_in_module(module.clone())?);
            }
        } else {
            items.push(parser.parse_program_item_in_module(MAIN_BRANCH.to_string())?);
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

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Token::Eof)
    }

    fn expect_eof(&self) -> Result<()> {
        if self.at_eof() {
            Ok(())
        } else {
            bail!("unexpected token at end: {:?}", self.peek())
        }
    }

    fn parse_function(&mut self) -> Result<FunctionSource> {
        self.parse_function_in_module(MAIN_BRANCH.to_string())
    }

    fn parse_program_item_in_module(&mut self, module: String) -> Result<ProgramItem> {
        if self.consume_ident_value("extern") {
            Ok(ProgramItem::ExternalFunction(
                self.parse_external_function_in_module(module)?,
            ))
        } else {
            Ok(ProgramItem::Function(
                self.parse_function_in_module(module)?,
            ))
        }
    }

    fn parse_function_in_module(&mut self, module: String) -> Result<FunctionSource> {
        self.expect_ident_value("fn")?;
        let name = self.expect_ident()?;
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
            self.parse_case()
        }
    }

    fn parse_case(&mut self) -> Result<RawExpr> {
        if self.consume_ident_value("case") {
            let expr = self.parse_expr()?;
            self.expect_ident_value("of")?;
            let mut arms = Vec::new();
            loop {
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
                    variant,
                    binding,
                    body,
                });
                if !self.consume_symbol("|") {
                    break;
                }
            }
            Ok(RawExpr::Case {
                expr: Box::new(expr),
                arms,
            })
        } else {
            self.parse_binary_prec(1)
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
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<RawExpr> {
        match self.next() {
            Token::Number(value) => Ok(RawExpr::LiteralI64 { value }),
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
                if self.consume_symbol("(") {
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
            other => bail!("unexpected token in expression: {other:?}"),
        }
    }

    fn parse_type_source(&mut self) -> Result<String> {
        match self.next() {
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
            "bool" | "Bool" => Ok("bool".to_string()),
            "unit" | "Unit" => Ok("unit".to_string()),
            "record" => {
                let fields = self.parse_type_fields()?;
                Ok(format!("record {{{}}}", fields.join(", ")))
            }
            "enum" => {
                let variants = self.parse_type_fields()?;
                Ok(format!("enum {{{}}}", variants.join(", ")))
            }
            other => bail!("unknown type {other}"),
        }
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
        while self.consume_symbol(".") {
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

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn next(&mut self) -> Token {
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
            i += 1;
            let mut value = String::new();
            while i < chars.len() {
                match chars[i] {
                    '"' => {
                        i += 1;
                        break;
                    }
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
            if i > chars.len() || chars.get(i.saturating_sub(1)) != Some(&'"') {
                bail!("unterminated string literal");
            }
            tokens.push(Token::String(value));
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
