use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use crate::model::{aliases_for, param_names};
use crate::store::{CodeDb, canonical_json};
use crate::{DEFAULT_NATIVE_TARGET, MAIN_BRANCH};

pub const WORKSPACE_REQUEST_SCHEMA: &str = "codedb/request/v1";
pub const WORKSPACE_RESPONSE_SCHEMA: &str = "codedb/response/v1";

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceRequest {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub method: String,
    #[serde(default = "empty_params")]
    pub params: JsonValue,
    #[serde(default)]
    pub id: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub branch: String,
    pub root_hash: String,
    pub history_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDiagnostic {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceError {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceResponse {
    pub schema: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonValue>,
    pub diagnostics: Vec<WorkspaceDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<WorkspaceSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkspaceError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonValue>,
}

impl WorkspaceResponse {
    pub fn ok(
        result: JsonValue,
        diagnostics: Vec<WorkspaceDiagnostic>,
        snapshot: WorkspaceSnapshot,
        id: Option<JsonValue>,
    ) -> Self {
        Self {
            schema: WORKSPACE_RESPONSE_SCHEMA.to_string(),
            status: "ok".to_string(),
            result: Some(result),
            diagnostics,
            snapshot: Some(snapshot),
            error: None,
            id,
        }
    }

    pub fn error(
        kind: impl Into<String>,
        message: impl Into<String>,
        snapshot: Option<WorkspaceSnapshot>,
        id: Option<JsonValue>,
    ) -> Self {
        Self {
            schema: WORKSPACE_RESPONSE_SCHEMA.to_string(),
            status: "error".to_string(),
            result: None,
            diagnostics: Vec::new(),
            snapshot,
            error: Some(WorkspaceError {
                kind: kind.into(),
                message: message.into(),
            }),
            id,
        }
    }
}

pub fn workspace_response_json(response: &WorkspaceResponse) -> Result<String> {
    Ok(format!(
        "{}\n",
        canonical_json(&serde_json::to_value(response)?)
    ))
}

pub fn execute_workspace_request(db: &mut CodeDb, request: WorkspaceRequest) -> WorkspaceResponse {
    let id = request.id.clone();
    if let Some(schema) = &request.schema
        && schema != WORKSPACE_REQUEST_SCHEMA
    {
        return WorkspaceResponse::error(
            "invalid_request",
            format!("unsupported request schema {schema:?}; expected {WORKSPACE_REQUEST_SCHEMA}"),
            snapshot_or_none(db, MAIN_BRANCH),
            id,
        );
    }
    if let Some(jsonrpc) = &request.jsonrpc
        && jsonrpc != "2.0"
    {
        return WorkspaceResponse::error(
            "invalid_request",
            format!("unsupported JSON-RPC version {jsonrpc:?}; expected \"2.0\""),
            snapshot_or_none(db, MAIN_BRANCH),
            id,
        );
    }

    match dispatch_workspace_method(db, &request.method, &request.params) {
        Ok(method_result) => WorkspaceResponse::ok(
            method_result.result,
            method_result.diagnostics,
            method_result.snapshot,
            id,
        ),
        Err(err) => {
            WorkspaceResponse::error(err.kind, err.message, snapshot_or_none(db, MAIN_BRANCH), id)
        }
    }
}

struct WorkspaceMethodResult {
    result: JsonValue,
    diagnostics: Vec<WorkspaceDiagnostic>,
    snapshot: WorkspaceSnapshot,
}

impl WorkspaceMethodResult {
    fn new(result: JsonValue, snapshot: WorkspaceSnapshot) -> Self {
        Self {
            result,
            diagnostics: Vec::new(),
            snapshot,
        }
    }
}

#[derive(Debug)]
struct WorkspaceMethodError {
    kind: &'static str,
    message: String,
}

impl WorkspaceMethodError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn method(err: anyhow::Error) -> Self {
        Self::new("method_error", format!("{err:#}"))
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self::new("invalid_params", message)
    }
}

type MethodResult<T> = std::result::Result<T, WorkspaceMethodError>;

fn dispatch_workspace_method(
    db: &mut CodeDb,
    method: &str,
    params: &JsonValue,
) -> MethodResult<WorkspaceMethodResult> {
    match method {
        "workspace.current" => workspace_current(db, params),
        "workspace.branches" => workspace_branches(db, params),
        "symbols.list" => symbols_list(db, params),
        "symbols.show" => symbols_show(db, params),
        "symbols.resolve" => symbols_resolve(db, params),
        "symbols.callers" => symbols_callers(db, params),
        "roots.diff" => roots_diff(db, params),
        "roots.export_projection" => roots_export_projection(db, params),
        "build.plan" => build_plan(db, params),
        "history.list" => history_list(db, params),
        "verify.run" => verify_run(db, params),
        _ => Err(WorkspaceMethodError::new(
            "unknown_method",
            format!("unknown workspace method {method:?}"),
        )),
    }
}

fn workspace_current(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let snapshot = workspace_snapshot(db, &branch)?;
    Ok(WorkspaceMethodResult::new(
        serde_json::to_value(&snapshot)
            .map_err(|err| WorkspaceMethodError::new("serialization_error", err.to_string()))?,
        snapshot,
    ))
}

fn workspace_branches(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    params_object(params)?;
    let result = parse_json_payload(db.branches_json().map_err(WorkspaceMethodError::method)?)?;
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn symbols_list(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    require_main_branch(params, "symbols.list")?;
    let result = parse_json_payload(
        db.list_main_branch_json()
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn symbols_show(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    require_main_branch(params, "symbols.show")?;
    let symbol_or_name = symbol_or_name_param(params)?;
    let result = parse_json_payload(
        db.show_main_branch_json(&symbol_or_name)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn symbols_resolve(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    require_main_branch(params, "symbols.resolve")?;
    let object = params_object(params)?;
    let module = optional_str(object, "module")?.unwrap_or(MAIN_BRANCH);
    let branch = db
        .branch(MAIN_BRANCH)
        .map_err(WorkspaceMethodError::method)?;
    let root = db
        .load_root(&branch.root_hash)
        .map_err(WorkspaceMethodError::method)?;
    let (query, symbol) = if let Some(symbol) = optional_str(object, "symbol")? {
        (symbol.to_string(), symbol.to_string())
    } else if let Some(symbol_or_name) = optional_str(object, "symbol_or_name")? {
        (
            symbol_or_name.to_string(),
            db.resolve_symbol_or_name(&branch.root_hash, symbol_or_name)
                .map_err(WorkspaceMethodError::method)?,
        )
    } else if let Some(name) = optional_str(object, "name")? {
        (
            format!("{module}.{name}"),
            db.resolve_name(&branch.root_hash, module, name)
                .map_err(WorkspaceMethodError::method)?,
        )
    } else {
        return Err(WorkspaceMethodError::invalid_params(
            "symbols.resolve requires symbol, symbol_or_name, or name",
        ));
    };
    let root_symbol = db.root_symbol(&root, &symbol).ok_or_else(|| {
        WorkspaceMethodError::invalid_params(format!("symbol is not in root: {symbol}"))
    })?;
    let binding = db.preferred_binding(&root, &symbol).ok_or_else(|| {
        WorkspaceMethodError::new("method_error", format!("symbol has no name: {symbol}"))
    })?;
    let local_param_names = param_names(&root, &symbol);
    let result = json!({
        "branch": MAIN_BRANCH,
        "root_hash": branch.root_hash,
        "history_hash": branch.history_hash,
        "query": query,
        "module": binding.module,
        "name": binding.display_name,
        "aliases": aliases_for(&root, &symbol).into_iter().collect::<Vec<_>>(),
        "symbol_hash": symbol,
        "signature_hash": root_symbol.signature,
        "definition_hash": root_symbol.definition,
        "signature": db.signature_source(&root_symbol.signature, &local_param_names)
            .map_err(WorkspaceMethodError::method)?,
    });
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn symbols_callers(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    require_main_branch(params, "symbols.callers")?;
    let symbol_or_name = symbol_or_name_param(params)?;
    let branch = db
        .branch(MAIN_BRANCH)
        .map_err(WorkspaceMethodError::method)?;
    let root = db
        .load_root(&branch.root_hash)
        .map_err(WorkspaceMethodError::method)?;
    let symbol = db
        .resolve_symbol_or_name(&branch.root_hash, &symbol_or_name)
        .map_err(WorkspaceMethodError::method)?;
    let callers = db
        .direct_dependents_for_symbol(&branch.root_hash, &symbol)
        .map_err(WorkspaceMethodError::method)?
        .into_iter()
        .map(|caller| {
            Ok(json!({
                "name": db.symbol_display(&root, &caller)?,
                "symbol_hash": caller,
            }))
        })
        .collect::<Result<Vec<_>>>()
        .map_err(WorkspaceMethodError::method)?;
    let result = json!({
        "branch": MAIN_BRANCH,
        "root_hash": branch.root_hash,
        "history_hash": branch.history_hash,
        "symbol_hash": symbol,
        "callers": callers,
    });
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn roots_diff(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let object = params_object(params)?;
    let root_a = required_str_alias(object, "root_a", "old_root_hash")?;
    let root_b = required_str_alias(object, "root_b", "new_root_hash")?;
    let result = parse_json_payload(
        db.diff_roots_json(root_a, root_b)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn roots_export_projection(
    db: &mut CodeDb,
    params: &JsonValue,
) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let source = db
        .export_branch(&branch)
        .map_err(WorkspaceMethodError::method)?;
    let snapshot = workspace_snapshot(db, &branch)?;
    let result = json!({
        "branch": branch,
        "root_hash": snapshot.root_hash,
        "history_hash": snapshot.history_hash,
        "source": source,
    });
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn build_plan(db: &mut CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    require_main_branch(params, "build.plan")?;
    let object = params_object(params)?;
    let entry_name = required_str_alias(object, "entry_name", "entry")?;
    let target = optional_str(object, "target")?.unwrap_or(DEFAULT_NATIVE_TARGET);
    let result = parse_json_payload(
        db.build_plan_main_branch(entry_name, target)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn history_list(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    require_main_branch(params, "history.list")?;
    let result = parse_json_payload(
        db.history_main_branch_json()
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn verify_run(db: &mut CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    params_object(params)?;
    match db.verify() {
        Ok(message) => {
            let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
            Ok(WorkspaceMethodResult::new(
                json!({
                    "ok": true,
                    "message": message.trim_end(),
                }),
                snapshot,
            ))
        }
        Err(err) => Err(WorkspaceMethodError::new(
            "verify_failed",
            format!("{err:#}"),
        )),
    }
}

fn workspace_snapshot(db: &CodeDb, branch: &str) -> MethodResult<WorkspaceSnapshot> {
    let state = db.branch(branch).map_err(WorkspaceMethodError::method)?;
    Ok(WorkspaceSnapshot {
        branch: branch.to_string(),
        root_hash: state.root_hash,
        history_hash: state.history_hash,
    })
}

fn snapshot_or_none(db: &CodeDb, branch: &str) -> Option<WorkspaceSnapshot> {
    workspace_snapshot(db, branch).ok()
}

fn parse_json_payload(text: String) -> MethodResult<JsonValue> {
    serde_json::from_str(text.trim_end())
        .map_err(|err| WorkspaceMethodError::new("serialization_error", err.to_string()))
}

fn empty_params() -> JsonValue {
    json!({})
}

fn params_object(params: &JsonValue) -> MethodResult<&JsonMap<String, JsonValue>> {
    params
        .as_object()
        .ok_or_else(|| WorkspaceMethodError::invalid_params("params must be a JSON object"))
}

fn branch_param(params: &JsonValue) -> MethodResult<String> {
    let object = params_object(params)?;
    Ok(optional_str(object, "branch")?
        .unwrap_or(MAIN_BRANCH)
        .to_string())
}

fn require_main_branch(params: &JsonValue, method: &str) -> MethodResult<()> {
    let branch = branch_param(params)?;
    if branch == MAIN_BRANCH {
        Ok(())
    } else {
        Err(WorkspaceMethodError::invalid_params(format!(
            "{method} currently supports only branch {MAIN_BRANCH:?}, got {branch:?}"
        )))
    }
}

fn symbol_or_name_param(params: &JsonValue) -> MethodResult<String> {
    let object = params_object(params)?;
    if let Some(symbol_or_name) = optional_str(object, "symbol_or_name")? {
        return Ok(symbol_or_name.to_string());
    }
    if let Some(symbol) = optional_str(object, "symbol")? {
        return Ok(symbol.to_string());
    }
    if let Some(name) = optional_str(object, "name")? {
        return Ok(name.to_string());
    }
    Err(WorkspaceMethodError::invalid_params(
        "method requires symbol_or_name, symbol, or name",
    ))
}

fn optional_str<'a>(
    object: &'a JsonMap<String, JsonValue>,
    key: &str,
) -> MethodResult<Option<&'a str>> {
    object
        .get(key)
        .map(|value| {
            value.as_str().ok_or_else(|| {
                WorkspaceMethodError::invalid_params(format!("{key} must be a string"))
            })
        })
        .transpose()
}

fn required_str_alias<'a>(
    object: &'a JsonMap<String, JsonValue>,
    primary: &str,
    alias: &str,
) -> MethodResult<&'a str> {
    optional_str(object, primary)?
        .or(optional_str(object, alias)?)
        .ok_or_else(|| {
            WorkspaceMethodError::invalid_params(format!("method requires {primary} or {alias}"))
        })
}
