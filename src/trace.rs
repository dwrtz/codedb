use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::expr::{
    Value, ValueCell, array_cell, eval_binary, eval_unary, field_cell, slice_cells_from_array_cell,
    slice_len_from_value, subslice_value, value_cell,
};
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
pub struct TracePlace {
    pub root: String,
    pub index: usize,
    pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceValue {
    I64 {
        value: String,
    },
    Bool {
        value: bool,
    },
    Unit,
    Array {
        elements: Vec<TraceValue>,
    },
    Record {
        fields: Vec<TraceRecordField>,
    },
    Enum {
        variant: String,
        value: Box<TraceValue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRecordField {
    pub name: String,
    pub value: TraceValue,
}

impl TraceValue {
    fn from_value(value: &Value) -> Self {
        match value {
            Value::I64(value) => TraceValue::I64 {
                value: value.to_string(),
            },
            Value::U8(value) => TraceValue::I64 {
                value: value.to_string(),
            },
            Value::Bool(value) => TraceValue::Bool { value: *value },
            Value::Unit => TraceValue::Unit,
            Value::SharedRef(value) => TraceValue::Record {
                fields: vec![TraceRecordField {
                    name: "ref".to_string(),
                    value: TraceValue::from_value(&value.borrow()),
                }],
            },
            Value::MutRef(value) => TraceValue::Record {
                fields: vec![TraceRecordField {
                    name: "mut_ref".to_string(),
                    value: TraceValue::from_value(&value.borrow()),
                }],
            },
            Value::RawPtr { mutable, .. } => TraceValue::Record {
                fields: vec![TraceRecordField {
                    name: if *mutable {
                        "raw_mut_ptr".to_string()
                    } else {
                        "raw_ptr".to_string()
                    },
                    value: TraceValue::Unit,
                }],
            },
            Value::Slice { elements, .. } => TraceValue::Array {
                elements: elements
                    .iter()
                    .map(|value| TraceValue::from_value(&value.borrow()))
                    .collect(),
            },
            Value::Boxed(value) => TraceValue::Record {
                fields: vec![TraceRecordField {
                    name: "box".to_string(),
                    value: TraceValue::from_value(&value.borrow()),
                }],
            },
            Value::Vec { elements, capacity } => TraceValue::Record {
                fields: vec![
                    TraceRecordField {
                        name: "capacity".to_string(),
                        value: TraceValue::I64 {
                            value: capacity.to_string(),
                        },
                    },
                    TraceRecordField {
                        name: "elements".to_string(),
                        value: TraceValue::Array {
                            elements: elements
                                .iter()
                                .map(|value| TraceValue::from_value(&value.borrow()))
                                .collect(),
                        },
                    },
                ],
            },
            Value::String(bytes) => TraceValue::Record {
                fields: vec![
                    TraceRecordField {
                        name: "len".to_string(),
                        value: TraceValue::I64 {
                            value: bytes.len().to_string(),
                        },
                    },
                    TraceRecordField {
                        name: "bytes".to_string(),
                        value: TraceValue::Array {
                            elements: bytes
                                .iter()
                                .map(|byte| TraceValue::I64 {
                                    value: byte.to_string(),
                                })
                                .collect(),
                        },
                    },
                ],
            },
            Value::Array(elements) => TraceValue::Array {
                elements: elements
                    .iter()
                    .map(|value| TraceValue::from_value(&value.borrow()))
                    .collect(),
            },
            Value::Record(fields) => TraceValue::Record {
                fields: fields
                    .iter()
                    .map(|(name, value)| TraceRecordField {
                        name: name.clone(),
                        value: TraceValue::from_value(&value.borrow()),
                    })
                    .collect(),
            },
            Value::Enum { variant, value } => TraceValue::Enum {
                variant: variant.clone(),
                value: Box::new(TraceValue::from_value(&value.borrow())),
            },
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
    BorrowShared {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        place: TracePlace,
        region: String,
        referent_type_hash: String,
        type_hash: String,
    },
    BorrowMut {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        place: TracePlace,
        region: String,
        referent_type_hash: String,
        type_hash: String,
    },
    FieldAccess {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        place: TracePlace,
        field: String,
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
    CaseDecision {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        scrutinee: TraceValue,
        selected_variant: String,
        selected_expr_hash: String,
        payload: TraceValue,
    },
    LoopIteration {
        root_hash: String,
        frame: usize,
        symbol_hash: String,
        function_def_hash: String,
        expr_hash: String,
        iteration: usize,
        item: TraceValue,
        accumulator_before: TraceValue,
        accumulator_after: TraceValue,
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

    pub(crate) fn trace_root_text_args_report(
        &self,
        root_hash: &str,
        history_hash: Option<String>,
        entry_name: &str,
        args: &[String],
    ) -> Result<TraceReport> {
        let root = self.load_root(root_hash)?;
        let mut state = TraceState::new("root", root_hash.to_string(), history_hash, root);
        self.trace_root_text_args(&mut state, entry_name, args)
    }

    fn trace_root_text_args(
        &self,
        state: &mut TraceState,
        entry_name: &str,
        args: &[String],
    ) -> Result<TraceReport> {
        let entry_label = entry_name.to_string();
        let entry_symbol = match self.resolve_symbol_or_name(&state.root_hash, entry_name) {
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
            .map(|(idx, (arg, type_hash))| {
                let type_name = self.type_name(type_hash)?;
                parse_eval_arg(arg, &type_name, idx)
            })
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
            if !self.value_has_type(&state.root, arg, ty)? {
                bail!(
                    "argument {idx} has wrong type for {}: expected {}, got {arg}",
                    self.qualified_symbol_display(&state.root, symbol)?,
                    self.type_name(ty)?,
                );
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

        if self.definition_is_external(&root_symbol.definition)? {
            bail!("cannot trace external function {function_name}");
        }
        let body = self.function_body_hash(&root_symbol.definition)?;
        let mut args = args.into_iter().map(value_cell).collect::<Vec<_>>();
        let mut locals = Vec::new();
        let value = self.trace_expr(
            state,
            frame,
            symbol,
            &root_symbol.definition,
            &body,
            &mut args,
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
        args: &mut Vec<ValueCell>,
        locals: &mut Vec<ValueCell>,
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
            type_hash: type_hash.clone(),
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
            "static_bytes" => {
                let data_hash = payload
                    .get("static_data")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("static_bytes missing static_data"))?;
                let data = self.static_data_bytes(data_hash)?;
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    Ok(Value::Slice {
                        elements: data.into_iter().map(Value::U8).map(value_cell).collect(),
                        mutable: false,
                    }),
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
                            .map(|value| value.borrow().clone())
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
                            .map(|value| value.borrow().clone())
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
                state.events.push(TraceEvent::BorrowShared {
                    root_hash: state.root_hash.clone(),
                    frame,
                    symbol_hash: symbol_hash.to_string(),
                    function_def_hash: function_def_hash.to_string(),
                    expr_hash: expr_hash.to_string(),
                    place: self.trace_place_for_expr(target_hash, locals.len())?,
                    region,
                    referent_type_hash,
                    type_hash: type_hash.clone(),
                });
                let value = self
                    .trace_place_cell(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        target_hash,
                        args,
                        locals,
                    )
                    .map(Value::SharedRef);
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
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
                state.events.push(TraceEvent::BorrowMut {
                    root_hash: state.root_hash.clone(),
                    frame,
                    symbol_hash: symbol_hash.to_string(),
                    function_def_hash: function_def_hash.to_string(),
                    expr_hash: expr_hash.to_string(),
                    place: self.trace_place_for_expr(target_hash, locals.len())?,
                    region,
                    referent_type_hash,
                    type_hash: type_hash.clone(),
                });
                let value = self
                    .trace_place_cell(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        target_hash,
                        args,
                        locals,
                    )
                    .map(Value::MutRef);
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
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
                let source = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    value_hash,
                    args,
                    locals,
                )?;
                let value = match source {
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
                };
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
            }
            "raw_load" => {
                let pointer_hash = payload
                    .get("pointer")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("raw_load missing pointer"))?;
                let pointer = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    pointer_hash,
                    args,
                    locals,
                )?;
                let value = match pointer {
                    Value::RawPtr { target, .. } => Ok(target.borrow().clone()),
                    other => bail!("raw_load evaluated non-pointer source {other}"),
                };
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
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
                let pointer = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    pointer_hash,
                    args,
                    locals,
                )?;
                let value = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    value_hash,
                    args,
                    locals,
                )?;
                match pointer {
                    Value::RawPtr {
                        target,
                        mutable: true,
                    } => {
                        *target.borrow_mut() = value;
                        self.finish_current_expr(
                            state,
                            frame,
                            symbol_hash,
                            function_def_hash,
                            expr_hash,
                            Ok(Value::Unit),
                        )
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
                let value = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    value_hash,
                    args,
                    locals,
                )?;
                *self
                    .trace_place_cell(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        target_hash,
                        args,
                        locals,
                    )?
                    .borrow_mut() = value;
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    Ok(Value::Unit),
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
                locals.push(value_cell(value));
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
                        value: TraceValue::from_value(&value.borrow()),
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
                let item_name = payload
                    .get("item_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing item_name"))?
                    .to_string();
                let acc_name = payload
                    .get("acc_name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing acc_name"))?
                    .to_string();
                let element_type = payload
                    .get("element_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing element_type"))?
                    .to_string();
                let acc_type = payload
                    .get("acc_type")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("fold missing acc_type"))?
                    .to_string();
                let target = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    target_hash,
                    args,
                    locals,
                )?;
                let elements = match target {
                    Value::Array(elements) | Value::Slice { elements, .. } => elements,
                    other => bail!("fold target is not an array or slice: {other}"),
                };
                let mut accumulator = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    init_hash,
                    args,
                    locals,
                )?;
                for (iteration, item_cell) in elements.iter().enumerate() {
                    let item = item_cell.borrow().clone();
                    locals.push(value_cell(item.clone()));
                    locals.push(value_cell(accumulator.clone()));
                    state.events.push(TraceEvent::LocalBind {
                        root_hash: state.root_hash.clone(),
                        frame,
                        symbol_hash: symbol_hash.to_string(),
                        function_def_hash: function_def_hash.to_string(),
                        expr_hash: expr_hash.to_string(),
                        name: item_name.clone(),
                        type_hash: element_type.clone(),
                        value: TraceValue::from_value(&item),
                    });
                    state.events.push(TraceEvent::LocalBind {
                        root_hash: state.root_hash.clone(),
                        frame,
                        symbol_hash: symbol_hash.to_string(),
                        function_def_hash: function_def_hash.to_string(),
                        expr_hash: expr_hash.to_string(),
                        name: acc_name.clone(),
                        type_hash: acc_type.clone(),
                        value: TraceValue::from_value(&accumulator),
                    });
                    let before = accumulator.clone();
                    let body = self.trace_expr(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        body_hash,
                        args,
                        locals,
                    );
                    let acc_local = locals.pop();
                    let item_local = locals.pop();
                    if let Some(value) = acc_local {
                        state.events.push(TraceEvent::LocalUnbind {
                            root_hash: state.root_hash.clone(),
                            frame,
                            symbol_hash: symbol_hash.to_string(),
                            function_def_hash: function_def_hash.to_string(),
                            expr_hash: expr_hash.to_string(),
                            name: acc_name.clone(),
                            type_hash: acc_type.clone(),
                            value: TraceValue::from_value(&value.borrow()),
                        });
                    }
                    if let Some(value) = item_local {
                        state.events.push(TraceEvent::LocalUnbind {
                            root_hash: state.root_hash.clone(),
                            frame,
                            symbol_hash: symbol_hash.to_string(),
                            function_def_hash: function_def_hash.to_string(),
                            expr_hash: expr_hash.to_string(),
                            name: item_name.clone(),
                            type_hash: element_type.clone(),
                            value: TraceValue::from_value(&value.borrow()),
                        });
                    }
                    let after = body?;
                    state.events.push(TraceEvent::LoopIteration {
                        root_hash: state.root_hash.clone(),
                        frame,
                        symbol_hash: symbol_hash.to_string(),
                        function_def_hash: function_def_hash.to_string(),
                        expr_hash: expr_hash.to_string(),
                        iteration,
                        item: TraceValue::from_value(&item),
                        accumulator_before: TraceValue::from_value(&before),
                        accumulator_after: TraceValue::from_value(&after),
                    });
                    accumulator = after;
                }
                state.push_value(
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    &accumulator,
                );
                Ok(accumulator)
            }
            "record_literal" => {
                let mut values = std::collections::BTreeMap::new();
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
                        value_cell(self.trace_expr(
                            state,
                            frame,
                            symbol_hash,
                            function_def_hash,
                            value_hash,
                            args,
                            locals,
                        )?),
                    );
                }
                let value = Value::Record(values);
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
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
                    values.push(value_cell(self.trace_expr(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        value_hash,
                        args,
                        locals,
                    )?));
                }
                let value = Value::Array(values);
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
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
                let target = self.trace_place_cell(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    target_hash,
                    args,
                    locals,
                )?;
                let value = Value::Slice {
                    elements: slice_cells_from_array_cell(&target)?,
                    mutable,
                };
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
            }
            "slice_len" => {
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("slice_len missing target"))?;
                let target = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    target_hash,
                    args,
                    locals,
                )?;
                let value = Value::I64(slice_len_from_value(&target)? as i64);
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
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
                let target = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    target_hash,
                    args,
                    locals,
                )?;
                let start = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    start_hash,
                    args,
                    locals,
                )?;
                let len = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    len_hash,
                    args,
                    locals,
                )?;
                let value = subslice_value(
                    &target,
                    trace_index_value(&start)?,
                    trace_index_value(&len)?,
                )?;
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
            }
            "field_access" => {
                let field = payload
                    .get("field")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("field_access missing field"))?
                    .to_string();
                state.events.push(TraceEvent::FieldAccess {
                    root_hash: state.root_hash.clone(),
                    frame,
                    symbol_hash: symbol_hash.to_string(),
                    function_def_hash: function_def_hash.to_string(),
                    expr_hash: expr_hash.to_string(),
                    place: self.trace_place_for_expr(expr_hash, locals.len())?,
                    field,
                    type_hash: type_hash.clone(),
                });
                let value = self
                    .trace_place_cell(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        expr_hash,
                        args,
                        locals,
                    )
                    .map(|value| value.borrow().clone());
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
            }
            "array_index" => {
                let place_value = self
                    .trace_place_cell(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        expr_hash,
                        args,
                        locals,
                    )
                    .map(|value| value.borrow().clone());
                if place_value.is_ok() {
                    return self.finish_current_expr(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        expr_hash,
                        place_value,
                    );
                }
                let target_hash = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let index_hash = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing index"))?;
                let target = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    target_hash,
                    args,
                    locals,
                )?;
                let index = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    index_hash,
                    args,
                    locals,
                )?;
                let value = trace_array_value_cell(&target, trace_index_value(&index)?)
                    .map(|value| value.borrow().clone());
                self.finish_current_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    expr_hash,
                    value,
                )
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
                let payload_value = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    value_hash,
                    args,
                    locals,
                )?;
                let value = Value::Enum {
                    variant,
                    value: value_cell(payload_value),
                };
                state.push_value(frame, symbol_hash, function_def_hash, expr_hash, &value);
                Ok(value)
            }
            "case" => {
                let scrutinee_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case missing expr"))?;
                let scrutinee = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    scrutinee_hash,
                    args,
                    locals,
                )?;
                let scrutinee_trace = TraceValue::from_value(&scrutinee);
                let arms = payload
                    .get("arms")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("case missing arms"))?;
                // Select the arm (and, for enums, the bound payload cell) by
                // scrutinee kind: an enum dispatches by variant; an i64/bool
                // scrutinee dispatches by literal pattern with a `_` fallback —
                // mirroring the reference evaluator (R14). Scalars never bind.
                let (arm, selected_label, binding_value) = match &scrutinee {
                    Value::Enum { variant, value } => {
                        let arm = arms
                            .iter()
                            .find(|arm| {
                                arm.get("variant").and_then(JsonValue::as_str) == Some(variant)
                            })
                            .or_else(|| {
                                arms.iter().find(|arm| {
                                    arm.get("default").and_then(JsonValue::as_bool) == Some(true)
                                })
                            })
                            .ok_or_else(|| anyhow!("case missing arm for variant {variant}"))?;
                        (arm, variant.clone(), Some(value.clone()))
                    }
                    Value::I64(scrutinee_value) => {
                        let arm = arms
                            .iter()
                            .find(|arm| {
                                arm.get("literal_i64")
                                    .and_then(JsonValue::as_str)
                                    .and_then(|literal| literal.parse::<i64>().ok())
                                    == Some(*scrutinee_value)
                            })
                            .or_else(|| {
                                arms.iter().find(|arm| {
                                    arm.get("default").and_then(JsonValue::as_bool) == Some(true)
                                })
                            })
                            .ok_or_else(|| {
                                anyhow!("scalar case missing arm for value {scrutinee_value}")
                            })?;
                        (arm, scrutinee_value.to_string(), None)
                    }
                    Value::Bool(scrutinee_value) => {
                        let arm = arms
                            .iter()
                            .find(|arm| {
                                arm.get("literal_bool").and_then(JsonValue::as_bool)
                                    == Some(*scrutinee_value)
                            })
                            .or_else(|| {
                                arms.iter().find(|arm| {
                                    arm.get("default").and_then(JsonValue::as_bool) == Some(true)
                                })
                            })
                            .ok_or_else(|| {
                                anyhow!("scalar case missing arm for value {scrutinee_value}")
                            })?;
                        (arm, scrutinee_value.to_string(), None)
                    }
                    other => bail!("case expression evaluated to non-enum/scalar {other}"),
                };
                let body_hash = arm
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("case arm missing body"))?;
                let payload_trace = match &binding_value {
                    Some(value) => TraceValue::from_value(&value.borrow()),
                    None => scrutinee_trace.clone(),
                };
                state.events.push(TraceEvent::CaseDecision {
                    root_hash: state.root_hash.clone(),
                    frame,
                    symbol_hash: symbol_hash.to_string(),
                    function_def_hash: function_def_hash.to_string(),
                    expr_hash: expr_hash.to_string(),
                    scrutinee: scrutinee_trace,
                    selected_variant: selected_label,
                    selected_expr_hash: body_hash.to_string(),
                    payload: payload_trace,
                });
                let value = if arm
                    .get("binding_name")
                    .and_then(JsonValue::as_str)
                    .is_some()
                {
                    let binding =
                        binding_value.ok_or_else(|| anyhow!("scalar case arm cannot bind a value"))?;
                    locals.push(binding);
                    let body = self.trace_expr(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        body_hash,
                        args,
                        locals,
                    );
                    locals.pop();
                    body?
                } else {
                    self.trace_expr(
                        state,
                        frame,
                        symbol_hash,
                        function_def_hash,
                        body_hash,
                        args,
                        locals,
                    )?
                };
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

    fn trace_place_cell(
        &self,
        state: &mut TraceState,
        frame: usize,
        symbol_hash: &str,
        function_def_hash: &str,
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
                let target = self.trace_place_cell(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    target,
                    args,
                    locals,
                )?;
                field_cell(&target, field)
            }
            "array_index" => {
                let target = payload
                    .get("target")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing target"))?;
                let index_hash = payload
                    .get("index")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("array_index missing index"))?;
                let target = self.trace_place_cell(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    target,
                    args,
                    locals,
                )?;
                let index = self.trace_expr(
                    state,
                    frame,
                    symbol_hash,
                    function_def_hash,
                    index_hash,
                    args,
                    locals,
                )?;
                array_cell(&target, trace_index_value(&index)?)
            }
            other => bail!("expression kind {other} is not an assignable place"),
        }
    }

    fn trace_place_for_expr(&self, expr_hash: &str, locals_len: usize) -> Result<TracePlace> {
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
                Ok(TracePlace {
                    root: "param".to_string(),
                    index,
                    path: Vec::new(),
                })
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                let index = locals_len
                    .checked_sub(depth + 1)
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds: {depth}"))?;
                Ok(TracePlace {
                    root: "local".to_string(),
                    index,
                    path: Vec::new(),
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
                let mut place = self.trace_place_for_expr(target, locals_len)?;
                place.path.push(field.to_string());
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
                let mut place = self.trace_place_for_expr(target, locals_len)?;
                place.path.push(trace_array_index_segment(self, index)?);
                Ok(place)
            }
            other => bail!("expression kind {other} is not a semantic place"),
        }
    }
}

fn trace_index_value(value: &Value) -> Result<usize> {
    match value {
        Value::I64(index) if *index >= 0 => Ok(*index as usize),
        Value::I64(index) => bail!("array index must be non-negative, got {index}"),
        other => bail!("array index must be i64, got {other}"),
    }
}

fn trace_array_value_cell(value: &Value, index: usize) -> Result<ValueCell> {
    match value {
        Value::Slice { elements, .. } => elements
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("slice index out of bounds: {index}")),
        Value::Array(elements) => elements
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("array index out of bounds: {index}")),
        other => bail!("array index target is not an array: {other}"),
    }
}

fn trace_array_index_segment(db: &CodeDb, expr_hash: &str) -> Result<String> {
    let payload = db.get_payload(expr_hash)?;
    if payload.get("expr_kind").and_then(JsonValue::as_str) == Some("literal_i64") {
        let value = payload
            .get("value")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("literal_i64 missing value"))?
            .parse::<i64>()?;
        if value >= 0 {
            return Ok(format!("[{value}]"));
        }
    }
    Ok("[*]".to_string())
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

fn local_at_depth_mut<T>(locals: &mut [T], depth: usize) -> Option<&mut T> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get_mut(idx))
}
