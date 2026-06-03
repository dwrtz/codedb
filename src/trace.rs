use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::expr::{Value, eval_binary, eval_unary};
use crate::model::ProgramRootPayload;
use crate::store::{CodeDb, canonical_json};
use crate::{MAIN_BRANCH, parse_eval_arg};

pub const TRACE_SCHEMA: &str = "codedb/trace/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceReport {
    pub schema: String,
    pub status: String,
    pub branch: String,
    pub root_hash: String,
    pub history_hash: Option<String>,
    pub entry_symbol: Option<String>,
    pub entry_name: String,
    pub args: Vec<TraceValue>,
    pub result: Option<TraceValue>,
    pub diagnostics: Vec<TraceDiagnostic>,
    pub events: Vec<TraceEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceDiagnostic {
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<TraceLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLocation {
    pub root_hash: String,
    pub frame: usize,
    pub symbol_hash: String,
    pub function_def_hash: String,
    pub expr_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceValue {
    I64 { value: String },
    Bool { value: bool },
    Unit,
}

impl TraceValue {
    fn from_value(value: &Value) -> Self {
        match value {
            Value::I64(value) => TraceValue::I64 {
                value: value.to_string(),
            },
            Value::Bool(value) => TraceValue::Bool { value: *value },
            Value::Unit => TraceValue::Unit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TraceEvent {
    EnterFunction {
        root_hash: String,
        frame: usize,
        parent_frame: Option<usize>,
        symbol_hash: String,
        function_name: String,
        function_def_hash: String,
        args: Vec<TraceValue>,
    },
    ExitFunction {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_name: String,
        function_def_hash: String,
        value: TraceValue,
    },
    EvalExpr {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        expr_kind: String,
        type_hash: String,
    },
    Value {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        value: TraceValue,
    },
    Call {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        callee_symbol_hash: String,
        callee_name: String,
        args: Vec<TraceValue>,
    },
    BranchDecision {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        condition_value: TraceValue,
        selected_branch: String,
        selected_expr_hash: String,
    },
    LocalBind {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        name: String,
        type_hash: String,
        value: TraceValue,
    },
    LocalUnbind {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        name: String,
        type_hash: String,
        value: TraceValue,
    },
    Trap {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        kind: String,
        message: String,
    },
}

struct TraceState {
    branch: String,
    root_hash: String,
    history_hash: Option<String>,
    root: ProgramRootPayload,
    events: Vec<TraceEvent>,
    next_frame: usize,
}

impl TraceState {
    fn new(
        branch: impl Into<String>,
        root_hash: impl Into<String>,
        history_hash: Option<String>,
        root: ProgramRootPayload,
    ) -> Self {
        Self {
            branch: branch.into(),
            root_hash: root_hash.into(),
            history_hash,
            root,
            events: Vec::new(),
            next_frame: 0,
        }
    }

    fn alloc_frame(&mut self) -> usize {
        let frame = self.next_frame;
        self.next_frame += 1;
        frame
    }

    fn push_value(
        &mut self,
        frame: usize,
        symbol_hash: &str,
        function_def_hash: &str,
        expr_hash: &str,
        value: &Value,
    ) {
        self.events.push(TraceEvent::Value {
            root_hash: self.root_hash.clone(),
            frame,
            symbol_hash: symbol_hash.to_string(),
            function_def_hash: function_def_hash.to_string(),
            expr_hash: expr_hash.to_string(),
            value: TraceValue::from_value(value),
        });
    }

    fn push_trap(
        &mut self,
        frame: usize,
        symbol_hash: &str,
        function_def_hash: &str,
        expr_hash: &str,
        kind: &str,
        message: String,
    ) {
        self.events.push(TraceEvent::Trap {
            root_hash: self.root_hash.clone(),
            frame,
            symbol_hash: symbol_hash.to_string(),
            function_def_hash: function_def_hash.to_string(),
            expr_hash: expr_hash.to_string(),
            kind: kind.to_string(),
            message,
        });
    }
}

impl CodeDb {
    pub fn trace_main_branch_text_args_json(
        &self,
        entry_name: &str,
        args: &[String],
    ) -> Result<String> {
        let report = self.trace_branch_text_args(MAIN_BRANCH, entry_name, args)?;
        Ok(format!(
            "{}\n",
            canonical_json(&serde_json::to_value(report)?)
        ))
    }

    pub(crate) fn trace_branch_text_args_json(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
    ) -> Result<String> {
        let report = self.trace_branch_text_args(branch_name, entry_name, args)?;
        Ok(format!(
            "{}\n",
            canonical_json(&serde_json::to_value(report)?)
        ))
    }

    pub fn trace_main_branch_text_args(
        &self,
        entry_name: &str,
        args: &[String],
    ) -> Result<TraceReport> {
        self.trace_branch_text_args(MAIN_BRANCH, entry_name, args)
    }

    pub(crate) fn trace_branch_text_args(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
    ) -> Result<TraceReport> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut state = TraceState::new(
            branch_name,
            branch.root_hash.clone(),
            branch.history_hash.clone(),
            root,
        );
        self.trace_root_text_args(&mut state, entry_name, args)
    }

    fn trace_root_text_args(
        &self,
        state: &mut TraceState,
        entry_name: &str,
        args: &[String],
    ) -> Result<TraceReport> {
        let entry_label = format!("{MAIN_BRANCH}.{entry_name}");
        let entry_symbol = match self.resolve_name(&state.root_hash, MAIN_BRANCH, entry_name) {
            Ok(symbol) => symbol,
            Err(err) => {
                return Ok(trace_report(
                    state,
                    "error",
                    None,
                    entry_label,
                    Vec::new(),
                    None,
                    vec![TraceDiagnostic {
                        kind: "invalid_entry".to_string(),
                        message: format!("{err:#}"),
                        location: None,
                    }],
                ));
            }
        };
        let entry_label = self.qualified_symbol_display(&state.root, &entry_symbol)?;
        let root_symbol = match self.root_symbol(&state.root, &entry_symbol) {
            Some(root_symbol) => root_symbol.clone(),
            None => {
                return Ok(trace_report(
                    state,
                    "error",
                    Some(entry_symbol),
                    entry_label,
                    Vec::new(),
                    None,
                    vec![TraceDiagnostic {
                        kind: "invalid_entry".to_string(),
                        message: format!("entry symbol missing from root: {entry_name}"),
                        location: None,
                    }],
                ));
            }
        };
        let (param_types, _) = self.signature_parts(&root_symbol.signature)?;
        if param_types.len() != args.len() {
            return Ok(trace_report(
                state,
                "error",
                Some(entry_symbol),
                entry_label,
                Vec::new(),
                None,
                vec![TraceDiagnostic {
                    kind: "invalid_arguments".to_string(),
                    message: format!(
                        "{entry_name} expects {} args, got {}",
                        param_types.len(),
                        args.len()
                    ),
                    location: None,
                }],
            ));
        }

        let parsed_args = match args
            .iter()
            .zip(param_types.iter())
            .enumerate()
            .map(|(idx, (arg, type_hash))| parse_eval_arg(arg, self.type_name(type_hash)?, idx))
            .collect::<Result<Vec<_>>>()
        {
            Ok(args) => args,
            Err(err) => {
                return Ok(trace_report(
                    state,
                    "error",
                    Some(entry_symbol),
                    entry_label,
                    Vec::new(),
                    None,
                    vec![TraceDiagnostic {
                        kind: "invalid_arguments".to_string(),
                        message: format!("{err:#}"),
                        location: None,
                    }],
                ));
            }
        };
        let trace_args = parsed_args
            .iter()
            .map(TraceValue::from_value)
            .collect::<Vec<_>>();

        match self.trace_symbol(state, &entry_symbol, parsed_args, None) {
            Ok(result) => Ok(trace_report(
                state,
                "ok",
                Some(entry_symbol),
                entry_label,
                trace_args,
                Some(TraceValue::from_value(&result)),
                Vec::new(),
            )),
            Err(err) => Ok(trace_report(
                state,
                "error",
                Some(entry_symbol),
                entry_label,
                trace_args,
                None,
                vec![trace_diagnostic_for_error(state, err)],
            )),
        }
    }

    fn trace_symbol(
        &self,
        state: &mut TraceState,
        symbol: &str,
        args: Vec<Value>,
        parent_frame: Option<usize>,
    ) -> Result<Value> {
        let root_symbol = state
            .root
            .symbols
            .iter()
            .find(|entry| entry.symbol == symbol)
            .cloned()
            .ok_or_else(|| anyhow!("missing symbol {symbol}"))?;
        let (param_types, _) = self.signature_parts(&root_symbol.signature)?;
        if param_types.len() != args.len() {
            bail!(
                "{} expects {} args, got {}",
                self.qualified_symbol_display(&state.root, symbol)?,
                param_types.len(),
                args.len()
            );
        }
        for (idx, (arg, ty)) in args.iter().zip(param_types.iter()).enumerate() {
            match (arg, self.type_name(ty)?) {
                (Value::I64(_), "i64") | (Value::Bool(_), "bool") | (Value::Unit, "unit") => {}
                _ => bail!(
                    "argument {idx} has wrong type for {}",
                    self.qualified_symbol_display(&state.root, symbol)?
                ),
            }
        }

        let frame = state.alloc_frame();
        let function_name = self.qualified_symbol_display(&state.root, symbol)?;
        state.events.push(TraceEvent::EnterFunction {
            root_hash: state.root_hash.clone(),
            frame,
            parent_frame,
            symbol_hash: symbol.to_string(),
            function_name: function_name.clone(),
            function_def_hash: root_symbol.definition.clone(),
            args: args.iter().map(TraceValue::from_value).collect(),
        });

        let body = self.function_body_hash(&root_symbol.definition)?;
        let mut locals = Vec::new();
        let value = self.trace_expr(
            state,
            frame,
            symbol,
            &root_symbol.definition,
            &body,
            &args,
            &mut locals,
        )?;
        state.events.push(TraceEvent::ExitFunction {
            root_hash: state.root_hash.clone(),
            frame,
            symbol_hash: symbol.to_string(),
            function_name,
            function_def_hash: root_symbol.definition,
            value: TraceValue::from_value(&value),
        });
        Ok(value)
    }

    #[allow(clippy::too_many_arguments)]
    fn trace_expr(
        &self,
        state: &mut TraceState,
        frame: usize,
        symbol_hash: &str,
        function_def_hash: &str,
        expr_hash: &str,
        args: &[Value],
        locals: &mut Vec<Value>,
    ) -> Result<Value> {
        let payload = match self.get_payload(expr_hash) {
            Ok(payload) => payload,
            Err(err) => {
                state.push_trap(
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    "invalid_expression",
                    format!("{err:#}"),
                );
                return Err(err);
            }
        };
        let expr_kind = match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some(kind) => kind.to_string(),
            None => {
                let err = anyhow!("expression missing expr_kind {expr_hash}");
                state.push_trap(
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    "invalid_expression",
                    format!("{err:#}"),
                );
                return Err(err);
            }
        };
        let type_hash = match payload.get("type").and_then(JsonValue::as_str) {
            Some(type_hash) => type_hash.to_string(),
            None => {
                let err = anyhow!("expression missing type {expr_hash}");
                state.push_trap(
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    "invalid_expression",
                    format!("{err:#}"),
                );
                return Err(err);
            }
        };
        state.events.push(TraceEvent::EvalExpr {
            root_hash: state.root_hash.clone(),
            frame,
            symbol_hash: symbol_hash.to_string(),
            function_def_hash: function_def_hash.to_string(),
            expr_hash: expr_hash.to_string(),
            expr_kind: expr_kind.clone(),
            type_hash,
        });

        let result = match expr_kind.as_str() {
            "literal_i64" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))
                    .and_then(|value| value.parse::<i64>().map(Value::I64).map_err(Into::into));
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
            }
            "literal_bool" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .map(Value::Bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"));
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
            }
            "literal_unit" => self.finish_current_expr(
                state,
                frame,
                symbol_hash,
                function_def_hash,
                expr_hash,
                Ok(Value::Unit),
            ),
            "param_ref" => {
                let value = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))
                    .and_then(|index| {
                        args.get(index as usize)
                            .cloned()
                            .ok_or_else(|| anyhow!("parameter index out of bounds: {index}"))
                    });
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
            }
            "local_ref" => {
                let value = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))
                    .and_then(|depth| {
                        local_at_depth(locals, depth as usize)
                            .cloned()
                            .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))
                    });
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
            }
            "call" => {
                let callee_symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?
                    .to_string();
                let arg_hashes = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                let mut call_args = Vec::with_capacity(arg_hashes.len());
                for arg in arg_hashes {
                    let hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    call_args.push(self.trace_expr(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        hash,
                        args,
                        locals,
                    )?);
                }
                let callee_name = self.qualified_symbol_display(&state.root, &callee_symbol)?;
                state.events.push(TraceEvent::Call {
                    root_hash: state.root_hash.clone(),
                    frame,
                    symbol_hash: symbol_hash.to_string(),
                    function_def_hash: function_def_hash.to_string(),
                    expr_hash: expr_hash.to_string(),
                    callee_symbol_hash: callee_symbol.clone(),
                    callee_name,
                    args: call_args.iter().map(TraceValue::from_value).collect(),
                });
                let event_len = state.events.len();
                let value = match self.trace_symbol(state, &callee_symbol, call_args, Some(frame)) {
                    Ok(value) => value,
                    Err(err) => {
                        if !events_since_include_trap(&state.events[event_len..]) {
                            state.push_trap(
                                frame,
                                symbol_hash,
                                function_def_hash,
                                expr_hash,
                                "call_failed",
                                format!("{err:#}"),
                            );
                        }
                        return Err(err);
                    }
                };
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
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
                let left = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    left_hash,
                    args,
                    locals,
                )?;
                let right = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    right_hash,
                    args,
                    locals,
                )?;
                let value = eval_binary(op, left, right);
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
            }
            "unary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?;
                let child_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?;
                let child = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    child_hash,
                    args,
                    locals,
                )?;
                let value = eval_unary(op, child);
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
            }
            "let" => {
                let name = payload
                    .get("binding_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("let missing binding_name"))?
                    .to_string();
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
                let value = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    value_hash,
                    args,
                    locals,
                )?;
                state.events.push(TraceEvent::LocalBind {
                    root_hash: state.root_hash.clone(),
                    frame,
                    symbol_hash: symbol_hash.to_string(),
                    function_def_hash: function_def_hash.to_string(),
                    expr_hash: expr_hash.to_string(),
                    name: name.clone(),
                    type_hash: binding_type.clone(),
                    value: TraceValue::from_value(&value),
                });
                locals.push(value);
                let body = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    body_hash,
                    args,
                    locals,
                );
                let popped = locals.pop();
                if let Some(value) = &popped {
                    state.events.push(TraceEvent::LocalUnbind {
                        root_hash: state.root_hash.clone(),
                        frame,
                        symbol_hash: symbol_hash.to_string(),
                        function_def_hash: function_def_hash.to_string(),
                        expr_hash: expr_hash.to_string(),
                        name,
                        type_hash: binding_type,
                        value: TraceValue::from_value(value),
                    });
                }
                let body = body?;
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &body);
                Ok(body)
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
                let cond = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    cond_hash,
                    args,
                    locals,
                )?;
                let (selected_branch, selected_hash) = match &cond {
                    Value::Bool(true) => ("then", then_hash),
                    Value::Bool(false) => ("else", else_hash),
                    other => {
                        let err = anyhow!("if condition evaluated to non-bool {other}");
                        state.push_trap(
                            frame,
                            symbol_hash,
                            function_def_hash,
                            expr_hash,
                            "type_error",
                            format!("{err:#}"),
                        );
                        return Err(err);
                    }
                };
                state.events.push(TraceEvent::BranchDecision {
                    root_hash: state.root_hash.clone(),
                    frame,
                    symbol_hash: symbol_hash.to_string(),
                    function_def_hash: function_def_hash.to_string(),
                    expr_hash: expr_hash.to_string(),
                    condition_value: TraceValue::from_value(&cond),
                    selected_branch: selected_branch.to_string(),
                    selected_expr_hash: selected_hash.to_string(),
                });
                let value = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    selected_hash,
                    args,
                    locals,
                )?;
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
            }
            other => {
                let err = anyhow!("unknown expression kind {other}");
                state.push_trap(
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    "invalid_expression",
                    format!("{err:#}"),
                );
                Err(err)
            }
        };

        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                if !matches!(state.events.last(), Some(TraceEvent::Trap { expr_hash: trap_expr, .. }) if trap_expr == expr_hash)
                {
                    state.push_trap(
                        frame,
                        symbol_hash,
                        function_def_hash,
                        expr_hash,
                        "trap",
                        format!("{err:#}"),
                    );
                }
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_current_expr(
        &self,
        state: &mut TraceState,
        frame: usize,
        symbol_hash: &str,
        function_def_hash: &str,
        expr_hash: &str,
        value: Result<Value>,
    ) -> Result<Value> {
        match value {
            Ok(value) => {
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
            }
            Err(err) => {
                state.push_trap(
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    "trap",
                    format!("{err:#}"),
                );
                Err(err)
            }
        }
    }

    fn qualified_symbol_display(&self, root: &ProgramRootPayload, symbol: &str) -> Result<String> {
        let binding = self
            .preferred_binding(root, symbol)
            .ok_or_else(|| anyhow!("symbol has no display name {symbol}"))?;
        Ok(format!("{}.{}", binding.module, binding.display_name))
    }
}

fn trace_report(
    state: &TraceState,
    status: &str,
    entry_symbol: Option<String>,
    entry_name: String,
    args: Vec<TraceValue>,
    result: Option<TraceValue>,
    diagnostics: Vec<TraceDiagnostic>,
) -> TraceReport {
    TraceReport {
        schema: TRACE_SCHEMA.to_string(),
        status: status.to_string(),
        branch: state.branch.clone(),
        root_hash: state.root_hash.clone(),
        history_hash: state.history_hash.clone(),
        entry_symbol,
        entry_name,
        args,
        result,
        diagnostics,
        events: state.events.clone(),
    }
}

fn trace_diagnostic_for_error(state: &TraceState, err: anyhow::Error) -> TraceDiagnostic {
    state
        .events
        .iter()
        .rev()
        .find_map(|event| {
            if let TraceEvent::Trap {
                root_hash,
                frame,
                symbol_hash,
                function_def_hash,
                expr_hash,
                kind,
                message,
            } = event
            {
                Some(TraceDiagnostic {
                    kind: kind.clone(),
                    message: message.clone(),
                    location: Some(TraceLocation {
                        root_hash: root_hash.clone(),
                        frame: *frame,
                        symbol_hash: symbol_hash.clone(),
                        function_def_hash: function_def_hash.clone(),
                        expr_hash: expr_hash.clone(),
                    }),
                })
            } else {
                None
            }
        })
        .unwrap_or_else(|| TraceDiagnostic {
            kind: "trap".to_string(),
            message: format!("{err:#}"),
            location: None,
        })
}

fn events_since_include_trap(events: &[TraceEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, TraceEvent::Trap { .. }))
}

fn local_at_depth<T>(locals: &[T], depth: usize) -> Option<&T> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
}
