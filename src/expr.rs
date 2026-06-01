use std::collections::BTreeSet;
use std::fmt::{self, Display};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::model::{ProgramRootPayload, param_names, preferred_names};
use crate::store::CodeDb;
use crate::types::ParamSpec;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RawExpr {
    LiteralI64 {
        value: String,
    },
    LiteralBool {
        value: bool,
    },
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
    If {
        cond: Box<RawExpr>,
        #[serde(rename = "then")]
        then_expr: Box<RawExpr>,
        #[serde(rename = "else")]
        else_expr: Box<RawExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSource {
    pub module: String,
    pub name: String,
    pub params: Vec<ParamSpec>,
    pub return_type: String,
    pub body: RawExpr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    I64(i64),
    Bool(bool),
    Unit,
}

impl Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::I64(value) => write!(f, "{value}"),
            Value::Bool(value) => write!(f, "{value}"),
            Value::Unit => write!(f, "()"),
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
            match (arg, self.type_name(ty)?) {
                (Value::I64(_), "i64") | (Value::Bool(_), "bool") | (Value::Unit, "unit") => {}
                _ => bail!(
                    "argument {idx} has wrong type for {}",
                    self.symbol_display(&root, symbol)?
                ),
            }
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
                    call_args.push(self.eval_expr(root_hash, hash, args)?);
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
                let left = self.eval_expr(root_hash, left_hash, args)?;
                let right = self.eval_expr(root_hash, right_hash, args)?;
                eval_binary(op, left, right)
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
                match self.eval_expr(root_hash, cond_hash, args)? {
                    Value::Bool(true) => self.eval_expr(root_hash, then_hash, args),
                    Value::Bool(false) => self.eval_expr(root_hash, else_hash, args),
                    other => bail!("if condition evaluated to non-bool {other}"),
                }
            }
            other => bail!("unknown expression kind {other}"),
        }
    }

    pub(crate) fn render_source(&self, root_hash: &str) -> Result<String> {
        let root = self.load_root(root_hash)?;
        let mut chunks = Vec::new();
        for binding in self.source_projection_order(&root)? {
            let symbol = binding.symbol;
            let root_symbol = self
                .root_symbol(&root, &symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {symbol}"))?;
            let body = self.function_body_hash(&root_symbol.definition)?;
            chunks.push(format!(
                "fn {}{} = {}",
                binding.display_name,
                self.signature_source(&root_symbol.signature, &param_names(&root, &symbol))?,
                self.expr_to_source(&body, &root, &param_names(&root, &symbol), 0)?
            ));
        }
        Ok(format!("{}\n", chunks.join("\n\n")))
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
        Ok(format!(
            "({}) -> {}",
            rendered_params.join(", "),
            self.type_name(&return_type)?
        ))
    }

    pub(crate) fn expr_to_source(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        local_params: &[String],
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
                        self.expr_to_source(hash, root, local_params, 0)
                    })
                    .collect::<Result<Vec<_>>>()?;
                format!(
                    "{}({})",
                    self.symbol_display(root, symbol)?,
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
                    self.expr_to_source(left, root, local_params, prec)?,
                    op,
                    self.expr_to_source(right, root, local_params, prec + 1)?
                );
                if prec < parent_prec {
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
                    self.expr_to_source(cond, root, local_params, 0)?,
                    self.expr_to_source(then_hash, root, local_params, 0)?,
                    self.expr_to_source(else_hash, root, local_params, 0)?
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
            "param_ref" => Ok(RawExpr::ParamRef {
                index: payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize,
            }),
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
                    name: self.symbol_display(root, symbol)?,
                    args: args
                        .iter()
                        .map(|arg| {
                            let hash = arg
                                .as_str()
                                .ok_or_else(|| anyhow!("call arg must be hash"))?;
                            self.typed_expr_to_raw(hash, root)
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
                    self.typed_expr_to_raw(
                        payload
                            .get("left")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing left"))?,
                        root,
                    )?,
                ),
                right: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("right")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing right"))?,
                        root,
                    )?,
                ),
            }),
            "if" => Ok(RawExpr::If {
                cond: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("cond")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing cond"))?,
                        root,
                    )?,
                ),
                then_expr: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("then")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing then"))?,
                        root,
                    )?,
                ),
                else_expr: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("else")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing else"))?,
                        root,
                    )?,
                ),
            }),
            other => bail!("unknown expression kind {other}"),
        }
    }
}

fn eval_binary(op: &str, left: Value, right: Value) -> Result<Value> {
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

impl CodeDb {
    pub(crate) fn dependencies_for_definition(
        &self,
        root: &ProgramRootPayload,
        definition_hash: &str,
    ) -> Result<BTreeSet<String>> {
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
            "literal_i64" | "literal_bool" | "param_ref" => {}
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
            "if" => {
                for key in ["cond", "then", "else"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("if missing {key}"))?;
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
    Symbol(String),
    Eof,
}

pub(crate) fn parse_program(source: &str) -> Result<Vec<FunctionSource>> {
    let mut parser = Parser::new(source)?;
    let mut functions = Vec::new();
    while !parser.at_eof() {
        functions.push(parser.parse_function()?);
    }
    Ok(functions)
}

pub(crate) fn parse_expr_source(source: &str) -> Result<RawExpr> {
    let mut parser = Parser::new(source)?;
    let expr = parser.parse_expr()?;
    parser.expect_eof()?;
    Ok(expr)
}

pub(crate) fn parse_signature_source(source: &str) -> Result<(Vec<ParamSpec>, String)> {
    let wrapped = format!("fn __sig__{source} = 0");
    let mut parser = Parser::new(&wrapped)?;
    let function = parser.parse_function()?;
    Ok((function.params, function.return_type))
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
        self.expect_ident_value("fn")?;
        let name = self.expect_ident()?;
        self.expect_symbol("(")?;
        let mut params = Vec::new();
        if !self.consume_symbol(")") {
            loop {
                let param_name = self.expect_ident()?;
                self.expect_symbol(":")?;
                let ty = self.expect_ident()?;
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
        let return_type = self.expect_ident_or_unit()?;
        self.expect_symbol("=")?;
        let body = self.parse_expr()?;
        Ok(FunctionSource {
            module: "main".to_string(),
            name,
            params,
            return_type,
            body,
        })
    }

    fn parse_expr(&mut self) -> Result<RawExpr> {
        self.parse_if()
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
            self.parse_binary_prec(1)
        }
    }

    fn parse_binary_prec(&mut self, min_prec: u8) -> Result<RawExpr> {
        let mut left = self.parse_primary()?;
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

    fn parse_primary(&mut self) -> Result<RawExpr> {
        match self.next() {
            Token::Number(value) => Ok(RawExpr::LiteralI64 { value }),
            Token::Ident(name) if name == "true" => Ok(RawExpr::LiteralBool { value: true }),
            Token::Ident(name) if name == "false" => Ok(RawExpr::LiteralBool { value: false }),
            Token::Ident(name) => {
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
                    Ok(RawExpr::Call { name, args })
                } else {
                    Ok(RawExpr::ParamName { name })
                }
            }
            Token::Symbol(symbol) if symbol == "(" => {
                let expr = self.parse_expr()?;
                self.expect_symbol(")")?;
                Ok(expr)
            }
            other => bail!("unexpected token in expression: {other:?}"),
        }
    }

    fn expect_ident(&mut self) -> Result<String> {
        match self.next() {
            Token::Ident(value) => Ok(value),
            other => bail!("expected identifier, got {other:?}"),
        }
    }

    fn expect_ident_or_unit(&mut self) -> Result<String> {
        if self.consume_symbol("(") {
            self.expect_symbol(")")?;
            Ok("unit".to_string())
        } else {
            self.expect_ident()
        }
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
        } else if i + 1 < chars.len() {
            let two = [chars[i], chars[i + 1]].iter().collect::<String>();
            if matches!(two.as_str(), "->" | "==" | "!=" | "<=" | ">=" | "&&" | "||") {
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
