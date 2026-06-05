use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::model::{ProgramRootPayload, param_names};
use crate::store::{CodeDb, canonical_json};
use crate::trace::{TraceDiagnostic, TraceEvent, TraceReport, TraceValue};

pub const DEBUG_SCHEMA: &str = "codedb/debug-session/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugCommand {
    Step,
    Next,
    Continue,
    BreakSymbol(String),
    BreakExpr(String),
    Backtrace,
    Where,
    PrintParams,
    PrintLocals,
    PrintValue(String),
    ShowExpr(Option<String>),
    ShowFunction,
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugReport {
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
    pub breakpoints: Vec<DebugBreakpoint>,
    pub current: DebugState,
    pub commands: Vec<DebugCommandRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugBreakpoint {
    pub id: usize,
    pub kind: String,
    pub target: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugCommandRecord {
    pub command: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonValue>,
    pub state: DebugState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugState {
    pub position: Option<usize>,
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<DebugEventView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame: Option<DebugFrame>,
    pub backtrace: Vec<DebugFrame>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugEventView {
    pub event_index: usize,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_def_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee_symbol_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<TraceValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<TraceValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugFrame {
    pub frame: usize,
    pub parent_frame: Option<usize>,
    pub symbol_hash: String,
    pub function_name: String,
    pub function_def_hash: String,
    pub params: Vec<DebugBinding>,
    pub locals: Vec<DebugBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugBinding {
    pub name: String,
    pub type_hash: String,
    pub type_name: String,
    pub value: TraceValue,
}

#[derive(Debug, Clone)]
pub struct DebugSession {
    trace: TraceReport,
    position: Option<usize>,
    breakpoints: Vec<DebugBreakpoint>,
    commands: Vec<DebugCommandRecord>,
    diagnostics: Vec<TraceDiagnostic>,
    quit: bool,
}

#[derive(Debug, Clone)]
struct RuntimeFrame {
    frame: usize,
    parent_frame: Option<usize>,
    symbol_hash: String,
    function_name: String,
    function_def_hash: String,
    args: Vec<TraceValue>,
    locals: Vec<RuntimeLocal>,
}

#[derive(Debug, Clone)]
struct RuntimeLocal {
    name: String,
    type_hash: String,
    value: TraceValue,
}

impl CodeDb {
    pub fn debug_session_main_branch_text_args(
        &self,
        entry_name: &str,
        args: &[String],
    ) -> Result<DebugSession> {
        self.debug_session_branch_text_args(crate::MAIN_BRANCH, entry_name, args)
    }

    pub(crate) fn debug_session_branch_text_args(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
    ) -> Result<DebugSession> {
        let trace = self.trace_branch_text_args(branch_name, entry_name, args)?;
        DebugSession::from_trace(trace)
    }

    pub fn debug_main_branch_text_args(
        &self,
        entry_name: &str,
        args: &[String],
        commands: &[String],
    ) -> Result<DebugReport> {
        self.debug_branch_text_args(crate::MAIN_BRANCH, entry_name, args, commands)
    }

    pub(crate) fn debug_branch_text_args(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
        commands: &[String],
    ) -> Result<DebugReport> {
        let mut session = self.debug_session_branch_text_args(branch_name, entry_name, args)?;
        session.run_commands(self, commands)
    }

    pub fn debug_main_branch_text_args_json(
        &self,
        entry_name: &str,
        args: &[String],
        commands: &[String],
    ) -> Result<String> {
        let report = self.debug_branch_text_args(crate::MAIN_BRANCH, entry_name, args, commands)?;
        Ok(format!(
            "{}\n",
            canonical_json(&serde_json::to_value(report)?)
        ))
    }

    pub(crate) fn debug_branch_text_args_json(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
        commands: &[String],
    ) -> Result<String> {
        let report = self.debug_branch_text_args(branch_name, entry_name, args, commands)?;
        Ok(format!(
            "{}\n",
            canonical_json(&serde_json::to_value(report)?)
        ))
    }
}

impl DebugSession {
    pub fn from_trace(trace: TraceReport) -> Result<Self> {
        let position = if trace.events.is_empty() {
            None
        } else {
            Some(0)
        };
        Ok(Self {
            diagnostics: trace.diagnostics.clone(),
            trace,
            position,
            breakpoints: Vec::new(),
            commands: Vec::new(),
            quit: false,
        })
    }

    pub fn run_commands(&mut self, db: &CodeDb, commands: &[String]) -> Result<DebugReport> {
        for command in commands {
            self.execute_command(db, command)?;
            if self.quit {
                break;
            }
        }
        self.report(db)
    }

    pub fn execute_command(&mut self, db: &CodeDb, input: &str) -> Result<DebugCommandRecord> {
        let command = input.trim().to_string();
        let record = match parse_debug_command(&command) {
            Ok(parsed) => match self.apply_command(db, &command, parsed) {
                Ok(record) => record,
                Err(err) => self.error_record(db, &command, format!("{err:#}"))?,
            },
            Err(err) => self.error_record(db, &command, format!("{err:#}"))?,
        };
        self.commands.push(record.clone());
        Ok(record)
    }

    pub fn report(&self, db: &CodeDb) -> Result<DebugReport> {
        let status = if self.trace.status == "error"
            || self.commands.iter().any(|record| record.status == "error")
        {
            "error"
        } else {
            "ok"
        };
        Ok(DebugReport {
            schema: DEBUG_SCHEMA.to_string(),
            status: status.to_string(),
            branch: self.trace.branch.clone(),
            root_hash: self.trace.root_hash.clone(),
            history_hash: self.trace.history_hash.clone(),
            entry_symbol: self.trace.entry_symbol.clone(),
            entry_name: self.trace.entry_name.clone(),
            args: self.trace.args.clone(),
            result: self.trace.result.clone(),
            diagnostics: self.diagnostics.clone(),
            breakpoints: self.breakpoints.clone(),
            current: self.current_state(db)?,
            commands: self.commands.clone(),
        })
    }

    pub fn is_quit(&self) -> bool {
        self.quit
    }

    pub fn current_state(&self, db: &CodeDb) -> Result<DebugState> {
        let Some(position) = self.position else {
            return Ok(DebugState {
                position: None,
                done: true,
                current: None,
                current_frame: None,
                backtrace: Vec::new(),
            });
        };
        self.state_at(db, position)
    }

    fn apply_command(
        &mut self,
        db: &CodeDb,
        command_text: &str,
        command: DebugCommand,
    ) -> Result<DebugCommandRecord> {
        match command {
            DebugCommand::Step => {
                let status = self.step_one();
                self.record(db, command_text, status, None, None)
            }
            DebugCommand::Next => {
                let status = self.next_one();
                self.record(db, command_text, status, None, None)
            }
            DebugCommand::Continue => {
                let result = self.continue_to_breakpoint();
                self.record(
                    db,
                    command_text,
                    result.status,
                    result.message,
                    result.result,
                )
            }
            DebugCommand::BreakSymbol(target) => {
                let target = self.resolve_symbol_breakpoint(db, &target)?;
                let status = if self.symbol_exists_in_root(db, &target)? {
                    "active"
                } else {
                    "obsolete"
                };
                let breakpoint = self.push_breakpoint("symbol", target, status);
                let command_status = if breakpoint.status == "obsolete" {
                    "obsolete_breakpoint"
                } else {
                    "ok"
                };
                self.record(
                    db,
                    command_text,
                    command_status,
                    None,
                    Some(json!({ "breakpoint": breakpoint })),
                )
            }
            DebugCommand::BreakExpr(target) => {
                if !target.starts_with("sha256:") {
                    bail!("expr breakpoint target must be an expression hash");
                }
                let status = if self.expr_exists_in_root(db, &target)? {
                    "active"
                } else {
                    "obsolete"
                };
                let breakpoint = self.push_breakpoint("expr", target, status);
                let command_status = if breakpoint.status == "obsolete" {
                    "obsolete_breakpoint"
                } else {
                    "ok"
                };
                self.record(
                    db,
                    command_text,
                    command_status,
                    None,
                    Some(json!({ "breakpoint": breakpoint })),
                )
            }
            DebugCommand::Backtrace => {
                let state = self.current_state(db)?;
                self.record(
                    db,
                    command_text,
                    "ok",
                    None,
                    Some(json!({ "frames": state.backtrace })),
                )
            }
            DebugCommand::Where => {
                let state = self.current_state(db)?;
                self.record(
                    db,
                    command_text,
                    "ok",
                    None,
                    Some(json!({ "current": state.current })),
                )
            }
            DebugCommand::PrintParams => {
                let state = self.current_state(db)?;
                let params = state
                    .current_frame
                    .as_ref()
                    .map(|frame| frame.params.clone())
                    .unwrap_or_default();
                self.record(
                    db,
                    command_text,
                    "ok",
                    None,
                    Some(json!({ "params": params })),
                )
            }
            DebugCommand::PrintLocals => {
                let state = self.current_state(db)?;
                let locals = state
                    .current_frame
                    .as_ref()
                    .map(|frame| frame.locals.clone())
                    .unwrap_or_default();
                self.record(
                    db,
                    command_text,
                    "ok",
                    None,
                    Some(json!({ "locals": locals })),
                )
            }
            DebugCommand::PrintValue(target) => {
                let value = self.debug_value(db, &target)?;
                self.record(
                    db,
                    command_text,
                    "ok",
                    None,
                    Some(json!({ "value": value })),
                )
            }
            DebugCommand::ShowExpr(explicit_hash) => {
                let state = self.current_state(db)?;
                let expr_hash = explicit_hash
                    .or_else(|| {
                        state
                            .current
                            .as_ref()
                            .and_then(|current| current.expr_hash.clone())
                    })
                    .ok_or_else(|| anyhow!("current debug event has no expression hash"))?;
                let symbol_hash = state
                    .current_frame
                    .as_ref()
                    .map(|frame| frame.symbol_hash.as_str());
                let view = self.expr_view(db, &expr_hash, symbol_hash)?;
                self.record(db, command_text, "ok", None, Some(json!({ "expr": view })))
            }
            DebugCommand::ShowFunction => {
                let state = self.current_state(db)?;
                let symbol_hash = state
                    .current_frame
                    .as_ref()
                    .map(|frame| frame.symbol_hash.clone())
                    .ok_or_else(|| anyhow!("no current function frame"))?;
                let view = self.function_view(db, &symbol_hash)?;
                self.record(
                    db,
                    command_text,
                    "ok",
                    None,
                    Some(json!({ "function": view })),
                )
            }
            DebugCommand::Quit => {
                self.quit = true;
                self.record(db, command_text, "quit", None, None)
            }
        }
    }

    fn record(
        &self,
        db: &CodeDb,
        command: &str,
        status: &str,
        message: Option<String>,
        result: Option<JsonValue>,
    ) -> Result<DebugCommandRecord> {
        Ok(DebugCommandRecord {
            command: command.to_string(),
            status: status.to_string(),
            message,
            result,
            state: self.current_state(db)?,
        })
    }

    fn error_record(
        &self,
        db: &CodeDb,
        command: &str,
        message: String,
    ) -> Result<DebugCommandRecord> {
        Ok(DebugCommandRecord {
            command: command.to_string(),
            status: "error".to_string(),
            message: Some(message),
            result: None,
            state: self.current_state(db)?,
        })
    }

    fn step_one(&mut self) -> &'static str {
        let Some(position) = self.position else {
            return "completed";
        };
        if position + 1 < self.trace.events.len() {
            self.position = Some(position + 1);
            "ok"
        } else {
            "completed"
        }
    }

    fn next_one(&mut self) -> &'static str {
        let Some(position) = self.position else {
            return "completed";
        };
        let Some(frame) = event_frame(&self.trace.events[position]) else {
            return self.step_one();
        };
        for idx in (position + 1)..self.trace.events.len() {
            if event_frame(&self.trace.events[idx]) == Some(frame) {
                self.position = Some(idx);
                return "ok";
            }
        }
        "completed"
    }

    fn continue_to_breakpoint(&mut self) -> ContinueOutcome {
        let start = self.position.map(|idx| idx + 1).unwrap_or(0);
        for idx in start..self.trace.events.len() {
            if let Some(breakpoint) = self.matching_breakpoint(&self.trace.events[idx]) {
                self.position = Some(idx);
                return ContinueOutcome {
                    status: "hit_breakpoint",
                    message: None,
                    result: Some(json!({
                        "breakpoint": breakpoint,
                        "event_index": idx,
                    })),
                };
            }
        }
        if !self.trace.events.is_empty() {
            self.position = Some(self.trace.events.len() - 1);
        }
        ContinueOutcome {
            status: "completed",
            message: Some("no active breakpoint matched before trace completion".to_string()),
            result: None,
        }
    }

    fn matching_breakpoint(&self, event: &TraceEvent) -> Option<DebugBreakpoint> {
        self.breakpoints
            .iter()
            .find(|breakpoint| {
                if breakpoint.status != "active" {
                    return false;
                }
                match breakpoint.kind.as_str() {
                    "symbol" => {
                        matches!(event, TraceEvent::EnterFunction { symbol_hash, .. } if symbol_hash == &breakpoint.target)
                    }
                    "expr" => event_expr_hash(event) == Some(breakpoint.target.as_str()),
                    _ => false,
                }
            })
            .cloned()
    }

    fn push_breakpoint(&mut self, kind: &str, target: String, status: &str) -> DebugBreakpoint {
        let breakpoint = DebugBreakpoint {
            id: self.breakpoints.len() + 1,
            kind: kind.to_string(),
            target,
            status: status.to_string(),
        };
        self.breakpoints.push(breakpoint.clone());
        breakpoint
    }

    fn resolve_symbol_breakpoint(&self, db: &CodeDb, target: &str) -> Result<String> {
        if target.starts_with("sha256:") {
            return Ok(target.to_string());
        }
        db.resolve_symbol_or_name(&self.trace.root_hash, target)
    }

    fn symbol_exists_in_root(&self, db: &CodeDb, symbol_hash: &str) -> Result<bool> {
        let root = db.load_root(&self.trace.root_hash)?;
        Ok(db.root_symbol(&root, symbol_hash).is_some())
    }

    fn expr_exists_in_root(&self, db: &CodeDb, expr_hash: &str) -> Result<bool> {
        let root = db.load_root(&self.trace.root_hash)?;
        let mut seen = BTreeSet::new();
        for symbol in &root.symbols {
            let body_hash = db.function_body_hash(&symbol.definition)?;
            if expr_reachable_from(db, &body_hash, expr_hash, &mut seen)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn state_at(&self, db: &CodeDb, position: usize) -> Result<DebugState> {
        let root = db.load_root(&self.trace.root_hash)?;
        let mut frames = BTreeMap::<usize, RuntimeFrame>::new();

        for (idx, event) in self.trace.events.iter().enumerate().take(position + 1) {
            match event {
                TraceEvent::EnterFunction {
                    frame,
                    parent_frame,
                    symbol_hash,
                    function_name,
                    function_def_hash,
                    args,
                    ..
                } => {
                    frames.insert(
                        *frame,
                        RuntimeFrame {
                            frame: *frame,
                            parent_frame: *parent_frame,
                            symbol_hash: symbol_hash.clone(),
                            function_name: function_name.clone(),
                            function_def_hash: function_def_hash.clone(),
                            args: args.clone(),
                            locals: Vec::new(),
                        },
                    );
                }
                TraceEvent::ExitFunction { frame, .. } if idx < position => {
                    frames.remove(frame);
                }
                TraceEvent::LocalBind {
                    frame,
                    name,
                    type_hash,
                    value,
                    ..
                } => {
                    if let Some(frame) = frames.get_mut(frame) {
                        frame.locals.push(RuntimeLocal {
                            name: name.clone(),
                            type_hash: type_hash.clone(),
                            value: value.clone(),
                        });
                    }
                }
                TraceEvent::LocalUnbind { frame, name, .. } if idx < position => {
                    if let Some(frame) = frames.get_mut(frame)
                        && let Some(pos) =
                            frame.locals.iter().rposition(|local| local.name == *name)
                    {
                        frame.locals.remove(pos);
                    }
                }
                _ => {}
            }
        }

        let current = self
            .trace
            .events
            .get(position)
            .map(|event| event_view(position, event));
        let current_frame_id = current
            .as_ref()
            .and_then(|event| event.frame)
            .or_else(|| frames.keys().next_back().copied());
        let backtrace = current_frame_id
            .map(|frame| self.backtrace_from_frame(db, &root, &frames, frame))
            .transpose()?
            .unwrap_or_default();
        let current_frame = backtrace.first().cloned();

        Ok(DebugState {
            position: Some(position),
            done: position + 1 >= self.trace.events.len(),
            current,
            current_frame,
            backtrace,
        })
    }

    fn backtrace_from_frame(
        &self,
        db: &CodeDb,
        root: &ProgramRootPayload,
        frames: &BTreeMap<usize, RuntimeFrame>,
        frame: usize,
    ) -> Result<Vec<DebugFrame>> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = Some(frame);
        while let Some(frame_id) = current {
            if !seen.insert(frame_id) {
                break;
            }
            let Some(frame) = frames.get(&frame_id) else {
                break;
            };
            out.push(frame_view(db, root, frame)?);
            current = frame.parent_frame;
        }
        Ok(out)
    }

    fn function_view(&self, db: &CodeDb, symbol_hash: &str) -> Result<JsonValue> {
        let root = db.load_root(&self.trace.root_hash)?;
        let root_symbol = db
            .root_symbol(&root, symbol_hash)
            .ok_or_else(|| anyhow!("symbol missing from root {symbol_hash}"))?;
        let body_hash = db.function_body_hash(&root_symbol.definition)?;
        let params = param_names(&root, symbol_hash);
        Ok(json!({
            "symbol_hash": symbol_hash,
            "name": db.symbol_display(&root, symbol_hash)?,
            "signature_hash": root_symbol.signature,
            "definition_hash": root_symbol.definition,
            "body_hash": body_hash,
            "signature": db.signature_source(&root_symbol.signature, &params)?,
            "body_source": db.expr_to_source(&body_hash, &root, &params, 0)?,
        }))
    }

    fn expr_view(
        &self,
        db: &CodeDb,
        expr_hash: &str,
        symbol_hash: Option<&str>,
    ) -> Result<JsonValue> {
        let root = db.load_root(&self.trace.root_hash)?;
        let payload = db.get_payload(expr_hash)?;
        let source = symbol_hash.and_then(|symbol_hash| {
            db.expr_to_source(expr_hash, &root, &param_names(&root, symbol_hash), 0)
                .ok()
        });
        Ok(json!({
            "expr_hash": expr_hash,
            "expr_kind": payload.get("expr_kind").and_then(JsonValue::as_str),
            "type_hash": payload.get("type").and_then(JsonValue::as_str),
            "source": source,
            "payload": payload,
        }))
    }

    fn debug_value(&self, db: &CodeDb, target: &str) -> Result<JsonValue> {
        if target.starts_with("sha256:") {
            let position = self
                .position
                .ok_or_else(|| anyhow!("debug session has no current position"))?;
            let (event_index, symbol_hash, value) = self
                .trace
                .events
                .iter()
                .enumerate()
                .take(position + 1)
                .rev()
                .find_map(|(idx, event)| match event {
                    TraceEvent::Value {
                        expr_hash,
                        symbol_hash,
                        value,
                        ..
                    } if expr_hash == target => Some((idx, symbol_hash.clone(), value.clone())),
                    _ => None,
                })
                .ok_or_else(|| {
                    anyhow!("value for expression {target} is not available at current position")
                })?;
            let view = self.expr_view(db, target, Some(&symbol_hash))?;
            return Ok(json!({
                "kind": "expr",
                "expr_hash": target,
                "symbol_hash": symbol_hash,
                "event_index": event_index,
                "value": value,
                "expr": view,
            }));
        }

        let state = self.current_state(db)?;
        let frame = state
            .current_frame
            .ok_or_else(|| anyhow!("debug session has no current frame"))?;
        frame
            .params
            .iter()
            .chain(frame.locals.iter())
            .find(|binding| binding.name == target)
            .map(|binding| {
                json!({
                    "kind": "binding",
                    "name": binding.name.clone(),
                    "type_hash": binding.type_hash.clone(),
                    "type_name": binding.type_name.clone(),
                    "value": binding.value.clone(),
                })
            })
            .ok_or_else(|| anyhow!("no parameter or local named {target:?} is in scope"))
    }
}

struct ContinueOutcome {
    status: &'static str,
    message: Option<String>,
    result: Option<JsonValue>,
}

pub fn parse_debug_command(input: &str) -> Result<DebugCommand> {
    let parts = input.split_whitespace().collect::<Vec<_>>();
    let Some(command) = parts.first().copied() else {
        bail!("debug command must not be empty");
    };
    match command {
        "step" | "s" if parts.len() == 1 => Ok(DebugCommand::Step),
        "next" | "n" if parts.len() == 1 => Ok(DebugCommand::Next),
        "continue" | "c" if parts.len() == 1 => Ok(DebugCommand::Continue),
        "backtrace" | "bt" if parts.len() == 1 => Ok(DebugCommand::Backtrace),
        "where" if parts.len() == 1 => Ok(DebugCommand::Where),
        "quit" | "q" if parts.len() == 1 => Ok(DebugCommand::Quit),
        "break" | "b" if parts.len() == 3 && parts[1] == "symbol" => {
            Ok(DebugCommand::BreakSymbol(parts[2].to_string()))
        }
        "break" | "b" if parts.len() == 3 && parts[1] == "expr" => {
            Ok(DebugCommand::BreakExpr(parts[2].to_string()))
        }
        "print" if parts.len() == 2 && parts[1] == "params" => Ok(DebugCommand::PrintParams),
        "print" if parts.len() == 2 && parts[1] == "locals" => Ok(DebugCommand::PrintLocals),
        "print" if parts.len() == 3 && parts[1] == "value" => {
            Ok(DebugCommand::PrintValue(parts[2].to_string()))
        }
        "show" if parts.len() == 2 && parts[1] == "expr" => Ok(DebugCommand::ShowExpr(None)),
        "show" if parts.len() == 3 && parts[1] == "expr" => {
            Ok(DebugCommand::ShowExpr(Some(parts[2].to_string())))
        }
        "show" if parts.len() == 2 && parts[1] == "function" => Ok(DebugCommand::ShowFunction),
        _ => bail!("unknown debug command: {input:?}"),
    }
}

fn frame_view(db: &CodeDb, root: &ProgramRootPayload, frame: &RuntimeFrame) -> Result<DebugFrame> {
    Ok(DebugFrame {
        frame: frame.frame,
        parent_frame: frame.parent_frame,
        symbol_hash: frame.symbol_hash.clone(),
        function_name: frame.function_name.clone(),
        function_def_hash: frame.function_def_hash.clone(),
        params: param_bindings(db, root, &frame.symbol_hash, &frame.args)?,
        locals: frame
            .locals
            .iter()
            .map(|local| {
                Ok(DebugBinding {
                    name: local.name.clone(),
                    type_name: db.type_name(&local.type_hash)?.to_string(),
                    type_hash: local.type_hash.clone(),
                    value: local.value.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn param_bindings(
    db: &CodeDb,
    root: &ProgramRootPayload,
    symbol_hash: &str,
    args: &[TraceValue],
) -> Result<Vec<DebugBinding>> {
    let root_symbol = db
        .root_symbol(root, symbol_hash)
        .ok_or_else(|| anyhow!("symbol missing from root {symbol_hash}"))?;
    let (param_types, _) = db.signature_parts(&root_symbol.signature)?;
    let names = param_names(root, symbol_hash);
    args.iter()
        .enumerate()
        .map(|(idx, value)| {
            let type_hash = param_types
                .get(idx)
                .ok_or_else(|| anyhow!("missing parameter type {idx} for {symbol_hash}"))?;
            Ok(DebugBinding {
                name: names.get(idx).cloned().unwrap_or_else(|| format!("p{idx}")),
                type_name: db.type_name(type_hash)?.to_string(),
                type_hash: type_hash.clone(),
                value: value.clone(),
            })
        })
        .collect()
}

fn expr_reachable_from(
    db: &CodeDb,
    current_hash: &str,
    target_hash: &str,
    seen: &mut BTreeSet<String>,
) -> Result<bool> {
    if current_hash == target_hash {
        return Ok(true);
    }
    if !seen.insert(current_hash.to_string()) {
        return Ok(false);
    }
    let payload = db.get_payload(current_hash)?;
    let expr_kind = payload
        .get("expr_kind")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("expression missing expr_kind {current_hash}"))?;
    match expr_kind {
        "literal_i64" | "literal_bool" | "literal_unit" | "param_ref" | "local_ref" => Ok(false),
        "call" => {
            for arg in payload
                .get("args")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| anyhow!("call missing args"))?
            {
                let child = arg
                    .as_str()
                    .ok_or_else(|| anyhow!("call arg must be hash"))?;
                if expr_reachable_from(db, child, target_hash, seen)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        "binary" => {
            if expr_reachable_child_field(db, &payload, "left", target_hash, seen)? {
                return Ok(true);
            }
            expr_reachable_child_field(db, &payload, "right", target_hash, seen)
        }
        "unary" => expr_reachable_child_field(db, &payload, "expr", target_hash, seen),
        "let" => {
            if expr_reachable_child_field(db, &payload, "value", target_hash, seen)? {
                return Ok(true);
            }
            expr_reachable_child_field(db, &payload, "body", target_hash, seen)
        }
        "if" => {
            if expr_reachable_child_field(db, &payload, "cond", target_hash, seen)? {
                return Ok(true);
            }
            if expr_reachable_child_field(db, &payload, "then", target_hash, seen)? {
                return Ok(true);
            }
            expr_reachable_child_field(db, &payload, "else", target_hash, seen)
        }
        other => bail!("unknown expression kind {other}"),
    }
}

fn expr_reachable_child_field(
    db: &CodeDb,
    payload: &JsonValue,
    key: &str,
    target_hash: &str,
    seen: &mut BTreeSet<String>,
) -> Result<bool> {
    let child = payload
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("expression missing {key}"))?;
    expr_reachable_from(db, child, target_hash, seen)
}

fn event_view(event_index: usize, event: &TraceEvent) -> DebugEventView {
    match event {
        TraceEvent::EnterFunction {
            frame,
            symbol_hash,
            function_name,
            function_def_hash,
            args,
            ..
        } => DebugEventView {
            event_index,
            event: "enter_function".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: Some(function_name.clone()),
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: None,
            expr_kind: None,
            type_hash: None,
            callee_symbol_hash: None,
            callee_name: None,
            value: None,
            args: Some(args.clone()),
            message: None,
        },
        TraceEvent::ExitFunction {
            frame,
            symbol_hash,
            function_name,
            function_def_hash,
            value,
            ..
        } => DebugEventView {
            event_index,
            event: "exit_function".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: Some(function_name.clone()),
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: None,
            expr_kind: None,
            type_hash: None,
            callee_symbol_hash: None,
            callee_name: None,
            value: Some(value.clone()),
            args: None,
            message: None,
        },
        TraceEvent::EvalExpr {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            expr_kind,
            type_hash,
            ..
        } => DebugEventView {
            event_index,
            event: "eval_expr".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: Some(expr_kind.clone()),
            type_hash: Some(type_hash.clone()),
            callee_symbol_hash: None,
            callee_name: None,
            value: None,
            args: None,
            message: None,
        },
        TraceEvent::BorrowShared {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            place,
            region,
            referent_type_hash,
            type_hash,
            ..
        } => DebugEventView {
            event_index,
            event: "borrow_shared".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: Some(type_hash.clone()),
            callee_symbol_hash: None,
            callee_name: None,
            value: None,
            args: None,
            message: Some(format!(
                "place {:?} region {region} referent {referent_type_hash}",
                place
            )),
        },
        TraceEvent::BorrowMut {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            place,
            region,
            referent_type_hash,
            type_hash,
            ..
        } => DebugEventView {
            event_index,
            event: "borrow_mut".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: Some(type_hash.clone()),
            callee_symbol_hash: None,
            callee_name: None,
            value: None,
            args: None,
            message: Some(format!(
                "place {:?} region {region} referent {referent_type_hash}",
                place
            )),
        },
        TraceEvent::FieldAccess {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            place,
            field,
            type_hash,
            ..
        } => DebugEventView {
            event_index,
            event: "field_access".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: Some(type_hash.clone()),
            callee_symbol_hash: None,
            callee_name: None,
            value: None,
            args: None,
            message: Some(format!("field {field} place {:?}", place)),
        },
        TraceEvent::Value {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            value,
            ..
        } => DebugEventView {
            event_index,
            event: "value".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: None,
            callee_symbol_hash: None,
            callee_name: None,
            value: Some(value.clone()),
            args: None,
            message: None,
        },
        TraceEvent::Call {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            callee_symbol_hash,
            callee_name,
            args,
            ..
        } => DebugEventView {
            event_index,
            event: "call".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: None,
            callee_symbol_hash: Some(callee_symbol_hash.clone()),
            callee_name: Some(callee_name.clone()),
            value: None,
            args: Some(args.clone()),
            message: None,
        },
        TraceEvent::BranchDecision {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            condition_value,
            selected_branch,
            selected_expr_hash,
            ..
        } => DebugEventView {
            event_index,
            event: "branch_decision".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: None,
            callee_symbol_hash: None,
            callee_name: None,
            value: Some(condition_value.clone()),
            args: None,
            message: Some(format!(
                "selected {selected_branch} branch {selected_expr_hash}"
            )),
        },
        TraceEvent::LocalBind {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            name,
            type_hash,
            value,
            ..
        } => DebugEventView {
            event_index,
            event: "local_bind".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: Some(type_hash.clone()),
            callee_symbol_hash: None,
            callee_name: None,
            value: Some(value.clone()),
            args: None,
            message: Some(format!("bound local {name}")),
        },
        TraceEvent::LocalUnbind {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            name,
            type_hash,
            value,
            ..
        } => DebugEventView {
            event_index,
            event: "local_unbind".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: Some(type_hash.clone()),
            callee_symbol_hash: None,
            callee_name: None,
            value: Some(value.clone()),
            args: None,
            message: Some(format!("unbound local {name}")),
        },
        TraceEvent::Trap {
            frame,
            symbol_hash,
            function_def_hash,
            expr_hash,
            kind,
            message,
            ..
        } => DebugEventView {
            event_index,
            event: "trap".to_string(),
            frame: Some(*frame),
            symbol_hash: Some(symbol_hash.clone()),
            function_name: None,
            function_def_hash: Some(function_def_hash.clone()),
            expr_hash: Some(expr_hash.clone()),
            expr_kind: None,
            type_hash: None,
            callee_symbol_hash: None,
            callee_name: None,
            value: None,
            args: None,
            message: Some(format!("{kind}: {message}")),
        },
    }
}

fn event_frame(event: &TraceEvent) -> Option<usize> {
    match event {
        TraceEvent::EnterFunction { frame, .. }
        | TraceEvent::ExitFunction { frame, .. }
        | TraceEvent::EvalExpr { frame, .. }
        | TraceEvent::BorrowShared { frame, .. }
        | TraceEvent::BorrowMut { frame, .. }
        | TraceEvent::FieldAccess { frame, .. }
        | TraceEvent::Value { frame, .. }
        | TraceEvent::Call { frame, .. }
        | TraceEvent::BranchDecision { frame, .. }
        | TraceEvent::LocalBind { frame, .. }
        | TraceEvent::LocalUnbind { frame, .. }
        | TraceEvent::Trap { frame, .. } => Some(*frame),
    }
}

fn event_expr_hash(event: &TraceEvent) -> Option<&str> {
    match event {
        TraceEvent::EvalExpr { expr_hash, .. }
        | TraceEvent::BorrowShared { expr_hash, .. }
        | TraceEvent::BorrowMut { expr_hash, .. }
        | TraceEvent::FieldAccess { expr_hash, .. }
        | TraceEvent::Value { expr_hash, .. }
        | TraceEvent::Call { expr_hash, .. }
        | TraceEvent::BranchDecision { expr_hash, .. }
        | TraceEvent::LocalBind { expr_hash, .. }
        | TraceEvent::LocalUnbind { expr_hash, .. }
        | TraceEvent::Trap { expr_hash, .. } => Some(expr_hash),
        TraceEvent::EnterFunction { .. } | TraceEvent::ExitFunction { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{DebugCommand, parse_debug_command};

    #[test]
    fn parses_non_interactive_debug_commands() {
        assert_eq!(parse_debug_command("step").unwrap(), DebugCommand::Step);
        assert_eq!(parse_debug_command("s").unwrap(), DebugCommand::Step);
        assert_eq!(parse_debug_command("next").unwrap(), DebugCommand::Next);
        assert_eq!(
            parse_debug_command("break symbol sha256:abc").unwrap(),
            DebugCommand::BreakSymbol("sha256:abc".to_string())
        );
        assert_eq!(
            parse_debug_command("break expr sha256:def").unwrap(),
            DebugCommand::BreakExpr("sha256:def".to_string())
        );
        assert_eq!(
            parse_debug_command("show expr sha256:def").unwrap(),
            DebugCommand::ShowExpr(Some("sha256:def".to_string()))
        );
        assert_eq!(
            parse_debug_command("print value sha256:def").unwrap(),
            DebugCommand::PrintValue("sha256:def".to_string())
        );
        assert!(parse_debug_command("print stack").is_err());
    }
}
