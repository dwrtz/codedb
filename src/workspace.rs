use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use crate::model::{BranchState, aliases_for, param_names};
use crate::store::{BranchDeleteOutcome, BranchFastForwardOutcome, CodeDb, canonical_json};
use crate::{DEFAULT_NATIVE_TARGET, MAIN_BRANCH};

pub const WORKSPACE_REQUEST_SCHEMA: &str = "codedb/request/v1";
pub const WORKSPACE_RESPONSE_SCHEMA: &str = "codedb/response/v1";
pub const WORKSPACE_TRANSACTION_SCHEMA: &str = "codedb/workspace-transaction/v1";
const APPLY_SCHEMA: &str = "codedb/apply/v1";

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
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkspaceTransaction {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(
        alias = "expected_root",
        alias = "expect_root",
        alias = "expect_root_hash"
    )]
    pub expected_root_hash: String,
    pub operations: Vec<JsonValue>,
    #[serde(default)]
    pub agent: Option<JsonValue>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceError {
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_root_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_root_hash: Option<String>,
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
        Self::error_with_details(kind, message, snapshot, id, None, None)
    }

    pub fn error_with_details(
        kind: impl Into<String>,
        message: impl Into<String>,
        snapshot: Option<WorkspaceSnapshot>,
        id: Option<JsonValue>,
        expected_root_hash: Option<String>,
        actual_root_hash: Option<String>,
    ) -> Self {
        Self::error_with_diagnostics_and_details(
            kind,
            message,
            Vec::new(),
            snapshot,
            id,
            expected_root_hash,
            actual_root_hash,
        )
    }

    pub fn error_with_diagnostics_and_details(
        kind: impl Into<String>,
        message: impl Into<String>,
        diagnostics: Vec<WorkspaceDiagnostic>,
        snapshot: Option<WorkspaceSnapshot>,
        id: Option<JsonValue>,
        expected_root_hash: Option<String>,
        actual_root_hash: Option<String>,
    ) -> Self {
        let expected_root = expected_root_hash.clone();
        let actual_root = actual_root_hash.clone();
        Self {
            schema: WORKSPACE_RESPONSE_SCHEMA.to_string(),
            status: "error".to_string(),
            result: None,
            diagnostics,
            snapshot,
            error: Some(WorkspaceError {
                kind: kind.into(),
                message: message.into(),
                expected_root,
                actual_root,
                expected_root_hash,
                actual_root_hash,
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

    let idempotency = match workspace_idempotency_request(&request) {
        Ok(idempotency) => idempotency,
        Err(err) => return workspace_method_error_response(db, err, id),
    };
    if let Some(idempotency) = &idempotency {
        match db.cached_workspace_transaction_response(
            &idempotency.request_id,
            &idempotency.request_hash,
        ) {
            Ok(Some(cached_response)) => {
                match serde_json::from_str::<WorkspaceResponse>(&cached_response) {
                    Ok(mut response) => {
                        response.id = id;
                        return response;
                    }
                    Err(err) => {
                        return WorkspaceResponse::error(
                            "method_error",
                            format!("cached workspace response is invalid JSON: {err}"),
                            snapshot_or_none(db, MAIN_BRANCH),
                            id,
                        );
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                return WorkspaceResponse::error(
                    "invalid_request",
                    format!("{err:#}"),
                    snapshot_or_none(db, MAIN_BRANCH),
                    id,
                );
            }
        }
    }

    match dispatch_workspace_method(db, &request.method, &request.params, idempotency.as_ref()) {
        Ok(method_result) => WorkspaceResponse::ok(
            method_result.result,
            method_result.diagnostics,
            method_result.snapshot,
            id.clone(),
        ),
        Err(err) => workspace_method_error_response(db, err, id.clone()),
    }
}

fn workspace_method_error_response(
    db: &CodeDb,
    err: WorkspaceMethodError,
    id: Option<JsonValue>,
) -> WorkspaceResponse {
    let WorkspaceMethodError(err) = err;
    let err = *err;
    WorkspaceResponse::error_with_diagnostics_and_details(
        err.kind,
        err.message,
        err.diagnostics,
        err.snapshot.or_else(|| snapshot_or_none(db, MAIN_BRANCH)),
        id,
        err.expected_root_hash,
        err.actual_root_hash,
    )
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
struct WorkspaceMethodError(Box<WorkspaceMethodErrorData>);

#[derive(Debug)]
struct WorkspaceMethodErrorData {
    kind: &'static str,
    message: String,
    diagnostics: Vec<WorkspaceDiagnostic>,
    snapshot: Option<WorkspaceSnapshot>,
    expected_root_hash: Option<String>,
    actual_root_hash: Option<String>,
}

impl WorkspaceMethodError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self(Box::new(WorkspaceMethodErrorData {
            kind,
            message: message.into(),
            diagnostics: Vec::new(),
            snapshot: None,
            expected_root_hash: None,
            actual_root_hash: None,
        }))
    }

    fn method(err: anyhow::Error) -> Self {
        Self::new("method_error", format!("{err:#}"))
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self::new("invalid_params", message)
    }

    fn stale_root(
        message: impl Into<String>,
        snapshot: WorkspaceSnapshot,
        expected_root_hash: impl Into<String>,
        actual_root_hash: impl Into<String>,
    ) -> Self {
        Self(Box::new(WorkspaceMethodErrorData {
            kind: "stale_root",
            message: message.into(),
            diagnostics: Vec::new(),
            snapshot: Some(snapshot),
            expected_root_hash: Some(expected_root_hash.into()),
            actual_root_hash: Some(actual_root_hash.into()),
        }))
    }

    fn with_diagnostics(mut self, diagnostics: Vec<WorkspaceDiagnostic>) -> Self {
        self.0.diagnostics = diagnostics;
        self
    }

    fn with_snapshot(mut self, snapshot: WorkspaceSnapshot) -> Self {
        self.0.snapshot = Some(snapshot);
        self
    }

    fn with_roots(
        mut self,
        expected_root_hash: Option<String>,
        actual_root_hash: Option<String>,
    ) -> Self {
        self.0.expected_root_hash = expected_root_hash;
        self.0.actual_root_hash = actual_root_hash;
        self
    }
}

type MethodResult<T> = std::result::Result<T, WorkspaceMethodError>;

struct WorkspaceIdempotencyRequest {
    method: String,
    request_id: String,
    request_hash: String,
    branch: String,
    expected_root_hash: Option<String>,
}

fn workspace_idempotency_request(
    request: &WorkspaceRequest,
) -> MethodResult<Option<WorkspaceIdempotencyRequest>> {
    if request.method != "ops.apply" {
        return Ok(None);
    }
    let Some(request_id) = workspace_request_id(request)? else {
        return Ok(None);
    };
    let request_hash = canonical_json(&json!({
        "method": &request.method,
        "params": &request.params,
    }));
    let (branch, expected_root_hash) = workspace_transaction_metadata(&request.params);
    Ok(Some(WorkspaceIdempotencyRequest {
        method: request.method.clone(),
        request_id,
        request_hash,
        branch,
        expected_root_hash,
    }))
}

fn workspace_request_id(request: &WorkspaceRequest) -> MethodResult<Option<String>> {
    if let Some(request_id) = &request.request_id {
        return validate_request_id(request_id).map(Some);
    }
    let Some(object) = request.params.as_object() else {
        return Ok(None);
    };
    if let Some(request_id) = request_id_from_value(object.get("request_id"))? {
        return Ok(Some(request_id));
    }
    if let Some(request_id) = request_id_from_value(object.get("idempotency_key"))? {
        return Ok(Some(request_id));
    }
    if let Some(agent) = object.get("agent").and_then(JsonValue::as_object)
        && let Some(request_id) = request_id_from_value(agent.get("request_id"))?
    {
        return Ok(Some(request_id));
    }
    if let Some(apply) = object.get("apply").and_then(JsonValue::as_object)
        && let Some(agent) = apply.get("agent").and_then(JsonValue::as_object)
        && let Some(request_id) = request_id_from_value(agent.get("request_id"))?
    {
        return Ok(Some(request_id));
    }
    Ok(None)
}

fn request_id_from_value(value: Option<&JsonValue>) -> MethodResult<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let request_id = value
        .as_str()
        .ok_or_else(|| WorkspaceMethodError::invalid_params("request_id must be a string"))?;
    validate_request_id(request_id).map(Some)
}

fn validate_request_id(request_id: &str) -> MethodResult<String> {
    if request_id.trim().is_empty() {
        return Err(WorkspaceMethodError::invalid_params(
            "request_id must not be empty",
        ));
    }
    Ok(request_id.to_string())
}

fn workspace_transaction_metadata(params: &JsonValue) -> (String, Option<String>) {
    let Ok(apply_document) = apply_document_param(params, false) else {
        return (MAIN_BRANCH.to_string(), None);
    };
    let branch = apply_document
        .as_object()
        .and_then(|object| object.get("branch"))
        .and_then(JsonValue::as_str)
        .unwrap_or(MAIN_BRANCH)
        .to_string();
    let expected_root_hash = expected_root_hash_from_apply_document(&apply_document)
        .ok()
        .flatten()
        .map(str::to_string);
    (branch, expected_root_hash)
}

fn dispatch_workspace_method(
    db: &mut CodeDb,
    method: &str,
    params: &JsonValue,
    idempotency: Option<&WorkspaceIdempotencyRequest>,
) -> MethodResult<WorkspaceMethodResult> {
    match method {
        "workspace.current" => workspace_current(db, params),
        "workspace.branches" => workspace_branches(db, params),
        "workspace.branch.create" => workspace_branch_create(db, params),
        "workspace.branch.fast_forward" => workspace_branch_fast_forward(db, params),
        "workspace.branch.delete" => workspace_branch_delete(db, params),
        "workspace.branch.compare" => workspace_branch_compare(db, params),
        "symbols.list" => symbols_list(db, params),
        "symbols.show" => symbols_show(db, params),
        "symbols.resolve" => symbols_resolve(db, params),
        "symbols.callers" => symbols_callers(db, params),
        "roots.diff" => roots_diff(db, params),
        "roots.export_projection" => roots_export_projection(db, params),
        "ops.apply" => ops_apply(db, params, idempotency),
        "ops.preview" => ops_preview(db, params),
        "build.plan" => build_plan(db, params),
        "build.execute" => build_execute(db, params),
        "build.artifact_status" => build_artifact_status(db, params),
        "trace.run" => trace_run(db, params),
        "debug.run" => debug_run(db, params),
        "tests.list" => tests_list(db, params),
        "tests.run" => tests_run(db, params),
        "tests.impact" => tests_impact(db, params),
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

fn workspace_branch_create(
    db: &mut CodeDb,
    params: &JsonValue,
) -> MethodResult<WorkspaceMethodResult> {
    let object = params_object(params)?;
    let name = required_str_any(object, &["name", "branch"])?;
    if db
        .branch_opt(name)
        .map_err(WorkspaceMethodError::method)?
        .is_some()
    {
        return Err(WorkspaceMethodError::new(
            "name_conflict",
            format!("branch already exists: {name}"),
        ));
    }
    let (source, source_state) = branch_source_state(db, object, Some(MAIN_BRANCH))?;
    let created = db
        .create_branch_pointer(
            name,
            &source_state.root_hash,
            source_state.history_hash.as_deref(),
        )
        .map_err(WorkspaceMethodError::method)?;
    let snapshot = workspace_snapshot(db, name)?;
    let result = json!({
        "schema": "codedb/branch-operation-result/v1",
        "status": "created",
        "branch": name,
        "root_hash": created.root_hash,
        "history_hash": created.history_hash,
        "source": source,
    });
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn workspace_branch_fast_forward(
    db: &mut CodeDb,
    params: &JsonValue,
) -> MethodResult<WorkspaceMethodResult> {
    let object = params_object(params)?;
    let target = required_str_any(object, &["branch", "target_branch", "target", "name"])?;
    let expected_root = required_str_any(
        object,
        &[
            "expect_root_hash",
            "expected_root_hash",
            "expect_root",
            "expected_root",
        ],
    )?;
    let (source, source_state) = branch_source_state(db, object, Some(MAIN_BRANCH))?;
    let outcome = db
        .fast_forward_branch_pointer(target, expected_root, &source_state)
        .map_err(WorkspaceMethodError::method)?;
    match outcome {
        BranchFastForwardOutcome::Updated { old, new } => {
            let snapshot = workspace_snapshot(db, target)?;
            let status = if old.root_hash == new.root_hash && old.history_hash == new.history_hash {
                "already_current"
            } else {
                "fast_forwarded"
            };
            let result = json!({
                "schema": "codedb/branch-operation-result/v1",
                "status": status,
                "branch": target,
                "old_root_hash": old.root_hash,
                "new_root_hash": new.root_hash,
                "old_history_hash": old.history_hash,
                "new_history_hash": new.history_hash,
                "source": source,
            });
            Ok(WorkspaceMethodResult::new(result, snapshot))
        }
        BranchFastForwardOutcome::StaleRoot { current } => {
            let snapshot = WorkspaceSnapshot {
                branch: target.to_string(),
                root_hash: current.root_hash.clone(),
                history_hash: current.history_hash,
            };
            Err(WorkspaceMethodError::stale_root(
                format!(
                    "branch {target:?} moved before fast-forward; expected root {expected_root}, actual root {}",
                    current.root_hash
                ),
                snapshot,
                expected_root,
                current.root_hash,
            ))
        }
        BranchFastForwardOutcome::NonFastForward {
            current,
            source: source_state,
        } => {
            let snapshot = WorkspaceSnapshot {
                branch: target.to_string(),
                root_hash: current.root_hash.clone(),
                history_hash: current.history_hash.clone(),
            };
            Err(WorkspaceMethodError::new(
                "dependency_conflict",
                format!(
                    "source root {} does not descend from branch {target:?} root {}",
                    source_state.root_hash, current.root_hash
                ),
            )
            .with_snapshot(snapshot)
            .with_roots(Some(current.root_hash), Some(source_state.root_hash)))
        }
    }
}

fn workspace_branch_delete(
    db: &mut CodeDb,
    params: &JsonValue,
) -> MethodResult<WorkspaceMethodResult> {
    let object = params_object(params)?;
    let name = required_str_any(object, &["name", "branch"])?;
    let expected_root = optional_str_any(
        object,
        &[
            "expect_root_hash",
            "expected_root_hash",
            "expect_root",
            "expected_root",
        ],
    )?;
    if name == MAIN_BRANCH {
        return Err(WorkspaceMethodError::new(
            "invalid_params",
            "workspace.branch.delete cannot delete the main branch",
        ));
    }
    match db
        .delete_branch_pointer_expected(name, expected_root)
        .map_err(WorkspaceMethodError::method)?
    {
        BranchDeleteOutcome::Deleted(deleted) => {
            let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
            let result = json!({
                "schema": "codedb/branch-operation-result/v1",
                "status": "deleted",
                "branch": name,
                "old_root_hash": deleted.root_hash,
                "old_history_hash": deleted.history_hash,
            });
            Ok(WorkspaceMethodResult::new(result, snapshot))
        }
        BranchDeleteOutcome::StaleRoot { current } => {
            let snapshot = WorkspaceSnapshot {
                branch: name.to_string(),
                root_hash: current.root_hash.clone(),
                history_hash: current.history_hash,
            };
            Err(WorkspaceMethodError::stale_root(
                format!(
                    "branch {name:?} moved before delete; expected root {}, actual root {}",
                    expected_root.unwrap_or(""),
                    current.root_hash
                ),
                snapshot,
                expected_root.unwrap_or(""),
                current.root_hash,
            ))
        }
    }
}

fn workspace_branch_compare(
    db: &CodeDb,
    params: &JsonValue,
) -> MethodResult<WorkspaceMethodResult> {
    let object = params_object(params)?;
    let branch_a = required_str_any(
        object,
        &["branch_a", "left", "from_branch", "old_branch", "base"],
    )?;
    let branch_b = required_str_any(
        object,
        &["branch_b", "right", "to_branch", "new_branch", "head"],
    )?;
    let result = parse_json_payload(
        db.compare_branches_json(branch_a, branch_b)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, branch_b)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn symbols_list(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let result = parse_json_payload(
        db.list_branch_json(&branch)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, &branch)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn symbols_show(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let symbol_or_name = symbol_or_name_param(params)?;
    let result = parse_json_payload(
        db.show_branch_json(&branch, &symbol_or_name)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, &branch)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn symbols_resolve(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch_name = branch_param(params)?;
    let object = params_object(params)?;
    let module = optional_str(object, "module")?.unwrap_or(MAIN_BRANCH);
    let branch = db
        .branch(&branch_name)
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
        "branch": branch_name,
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
    let snapshot = workspace_snapshot(db, &branch_name)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn symbols_callers(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch_name = branch_param(params)?;
    let symbol_or_name = symbol_or_name_param(params)?;
    let branch = db
        .branch(&branch_name)
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
        "branch": branch_name,
        "root_hash": branch.root_hash,
        "history_hash": branch.history_hash,
        "symbol_hash": symbol,
        "callers": callers,
    });
    let snapshot = workspace_snapshot(db, &branch_name)?;
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

fn ops_apply(
    db: &mut CodeDb,
    params: &JsonValue,
    idempotency: Option<&WorkspaceIdempotencyRequest>,
) -> MethodResult<WorkspaceMethodResult> {
    let apply_document = apply_document_param(params, true)?;
    let expected_root = expected_root_hash_from_apply_document(&apply_document)?
        .ok_or_else(|| WorkspaceMethodError::invalid_params("ops.apply requires expected_root"))?
        .to_string();
    let branch = apply_document
        .as_object()
        .and_then(|object| object.get("branch"))
        .and_then(JsonValue::as_str)
        .unwrap_or(MAIN_BRANCH);
    let current_snapshot = workspace_snapshot(db, branch)?;
    if current_snapshot.root_hash != expected_root {
        if let Some(cached_result) = cached_idempotent_apply_result(db, idempotency)? {
            return Ok(cached_result);
        }
        return Err(WorkspaceMethodError::stale_root(
            format!(
                "branch {branch:?} moved before ops.apply; expected root {expected_root}, actual root {}",
                current_snapshot.root_hash
            ),
            current_snapshot.clone(),
            expected_root,
            current_snapshot.root_hash,
        ));
    }

    let apply_text = canonical_json(&apply_document);
    let apply_response = if let Some(idempotency) = idempotency {
        db.apply_json_str_with_commit_hook(&apply_text, |db, apply_json| {
            record_idempotent_apply_response(db, idempotency, apply_json)
        })
    } else {
        db.apply_json_str(&apply_text)
    }
    .map_err(|err| WorkspaceMethodError::new("invalid_operation", format!("{err:#}")))?;
    let result = parse_json_payload(apply_response)?;
    let branch = result
        .get("branch")
        .and_then(JsonValue::as_str)
        .unwrap_or(MAIN_BRANCH);
    let snapshot =
        workspace_snapshot(db, branch).or_else(|_| workspace_snapshot(db, MAIN_BRANCH))?;
    if result.get("committed").and_then(JsonValue::as_bool) != Some(true)
        && let Some(cached_result) = cached_idempotent_apply_result(db, idempotency)?
    {
        return Ok(cached_result);
    }
    if let Some(err) = apply_result_workspace_error(&result, snapshot.clone()) {
        if let Some(cached_result) = cached_idempotent_apply_result(db, idempotency)? {
            return Ok(cached_result);
        }
        return Err(err);
    }
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn cached_idempotent_apply_result(
    db: &CodeDb,
    idempotency: Option<&WorkspaceIdempotencyRequest>,
) -> MethodResult<Option<WorkspaceMethodResult>> {
    let Some(idempotency) = idempotency else {
        return Ok(None);
    };
    let Some(cached_response) = db
        .cached_workspace_transaction_response(&idempotency.request_id, &idempotency.request_hash)
        .map_err(|err| WorkspaceMethodError::new("invalid_request", format!("{err:#}")))?
    else {
        return Ok(None);
    };
    let response = serde_json::from_str::<WorkspaceResponse>(&cached_response).map_err(|err| {
        WorkspaceMethodError::new(
            "method_error",
            format!("cached workspace response is invalid JSON: {err}"),
        )
    })?;
    let result = response.result.ok_or_else(|| {
        WorkspaceMethodError::new(
            "method_error",
            "cached workspace response is missing result",
        )
    })?;
    let snapshot = response.snapshot.ok_or_else(|| {
        WorkspaceMethodError::new(
            "method_error",
            "cached workspace response is missing snapshot",
        )
    })?;
    Ok(Some(WorkspaceMethodResult {
        result,
        diagnostics: response.diagnostics,
        snapshot,
    }))
}

fn ops_preview(db: &mut CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let apply_document = apply_document_param(params, false)?;
    let branch = apply_document
        .as_object()
        .and_then(|object| object.get("branch"))
        .and_then(JsonValue::as_str)
        .unwrap_or(MAIN_BRANCH);
    let result = parse_json_payload(
        db.preview_apply_json_str(&canonical_json(&apply_document))
            .map_err(|err| WorkspaceMethodError::new("invalid_operation", format!("{err:#}")))?,
    )?;
    let snapshot = workspace_snapshot(db, branch)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn record_idempotent_apply_response(
    db: &mut CodeDb,
    idempotency: &WorkspaceIdempotencyRequest,
    apply_json: &str,
) -> Result<()> {
    let result: JsonValue = serde_json::from_str(apply_json.trim_end())?;
    let branch = result
        .get("branch")
        .and_then(JsonValue::as_str)
        .unwrap_or(MAIN_BRANCH);
    let state = db.branch(branch)?;
    let snapshot = WorkspaceSnapshot {
        branch: branch.to_string(),
        root_hash: state.root_hash,
        history_hash: state.history_hash,
    };
    let response = WorkspaceResponse::ok(result, Vec::new(), snapshot, None);
    let response_json = canonical_json(&serde_json::to_value(&response)?);
    db.record_workspace_transaction_response(
        &idempotency.request_id,
        &idempotency.request_hash,
        &idempotency.method,
        &idempotency.branch,
        idempotency.expected_root_hash.as_deref(),
        &response_json,
    )
}

fn build_plan(db: &mut CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let object = params_object(params)?;
    let entry_name = required_str_alias(object, "entry_name", "entry")?;
    let target = optional_str(object, "target")?.unwrap_or(DEFAULT_NATIVE_TARGET);
    let result = parse_json_payload(
        db.build_plan_branch(&branch, entry_name, target)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, &branch)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn build_execute(db: &mut CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let object = params_object(params)?;
    let entry_name = required_str_alias(object, "entry_name", "entry")?;
    let target = optional_str(object, "target")?.unwrap_or(DEFAULT_NATIVE_TARGET);
    let build = match db.build_branch(&branch, entry_name, target) {
        Ok(build) => build,
        Err(err) => {
            let mut method_error = WorkspaceMethodError::method(err);
            if let Ok(snapshot) = workspace_snapshot(db, &branch) {
                method_error = method_error.with_snapshot(snapshot);
            }
            return Err(method_error);
        }
    };
    let snapshot = workspace_snapshot(db, &branch)?;
    let result = json!({
        "schema": "codedb/build-execute-result/v1",
        "branch": branch,
        "target_triple": target,
        "entry_name": entry_name,
        "executable_cache_key": build.cache_key,
        "executable_artifact_hash": build.artifact_hash,
        "executable_size_bytes": build.executable.len(),
    });
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn build_artifact_status(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    params_object(params)?;
    let result = parse_json_payload(
        db.artifact_status_json()
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, MAIN_BRANCH)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn trace_run(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let object = params_object(params)?;
    let entry_name = required_str_alias(object, "entry_name", "entry")?;
    let args = optional_string_array(object, "args")?.unwrap_or_default();
    let result = parse_json_payload(
        db.trace_branch_text_args_json(&branch, entry_name, &args)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let diagnostics = trace_workspace_diagnostics(&result);
    let snapshot = workspace_snapshot(db, &branch)?;
    if result.get("status").and_then(JsonValue::as_str) == Some("error") {
        return Err(nested_result_workspace_error(
            "trace_error",
            &result,
            diagnostics,
            snapshot,
        ));
    }
    Ok(WorkspaceMethodResult {
        result,
        diagnostics,
        snapshot,
    })
}

fn debug_run(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let object = params_object(params)?;
    let entry_name = required_str_alias(object, "entry_name", "entry")?;
    let args = optional_string_array(object, "args")?.unwrap_or_default();
    let commands = optional_string_array(object, "commands")?.unwrap_or_default();
    let result = parse_json_payload(
        db.debug_branch_text_args_json(&branch, entry_name, &args, &commands)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let diagnostics = trace_workspace_diagnostics(&result);
    let snapshot = workspace_snapshot(db, &branch)?;
    if result.get("status").and_then(JsonValue::as_str) == Some("error") {
        return Err(nested_result_workspace_error(
            "debug_error",
            &result,
            diagnostics,
            snapshot,
        ));
    }
    Ok(WorkspaceMethodResult {
        result,
        diagnostics,
        snapshot,
    })
}

fn tests_list(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let result = parse_json_payload(
        db.list_tests_branch_json(&branch)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, &branch)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn tests_run(db: &mut CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let result = parse_json_payload(
        db.run_tests_branch_json(&branch)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, &branch)?;
    if result.get("status").and_then(JsonValue::as_str) == Some("error") {
        return Err(nested_result_workspace_error(
            "test_error",
            &result,
            Vec::new(),
            snapshot,
        ));
    }
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn tests_impact(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let object = params_object(params)?;
    let old_root = required_str_any(object, &["old_root_hash", "old_root", "root_a"])?;
    let new_root = required_str_any(object, &["new_root_hash", "new_root", "root_b"])?;
    let result = parse_json_payload(
        db.test_impact_json(old_root, new_root)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, &branch)?;
    Ok(WorkspaceMethodResult::new(result, snapshot))
}

fn history_list(db: &CodeDb, params: &JsonValue) -> MethodResult<WorkspaceMethodResult> {
    let branch = branch_param(params)?;
    let result = parse_json_payload(
        db.history_branch_json(&branch)
            .map_err(WorkspaceMethodError::method)?,
    )?;
    let snapshot = workspace_snapshot(db, &branch)?;
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

fn default_branch() -> String {
    MAIN_BRANCH.to_string()
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

fn branch_source_state(
    db: &CodeDb,
    object: &JsonMap<String, JsonValue>,
    default_branch: Option<&str>,
) -> MethodResult<(JsonValue, BranchState)> {
    let source_branch = optional_str_any(object, &["from_branch", "source_branch", "from"])?;
    let source_root =
        optional_str_any(object, &["from_root_hash", "source_root_hash", "root_hash"])?;
    if source_branch.is_some() && source_root.is_some() {
        return Err(WorkspaceMethodError::invalid_params(
            "branch source must use either source_branch/from_branch or source_root_hash/from_root_hash, not both",
        ));
    }

    if let Some(root_hash) = source_root {
        db.load_root(root_hash)
            .map_err(WorkspaceMethodError::method)?;
        let history_hash = optional_str_any(
            object,
            &["from_history_hash", "source_history_hash", "history_hash"],
        )?
        .map(str::to_string);
        let source = json!({
            "kind": "root",
            "root_hash": root_hash,
            "history_hash": &history_hash,
        });
        return Ok((
            source,
            BranchState {
                root_hash: root_hash.to_string(),
                history_hash,
            },
        ));
    }

    let branch = source_branch.or(default_branch).ok_or_else(|| {
        WorkspaceMethodError::invalid_params(
            "branch source requires source_branch/from_branch or source_root_hash/from_root_hash",
        )
    })?;
    let state = db.branch(branch).map_err(|err| {
        WorkspaceMethodError::new(
            "branch_not_found",
            format!("source branch {branch:?}: {err:#}"),
        )
    })?;
    let source = json!({
        "kind": "branch",
        "branch": branch,
        "root_hash": &state.root_hash,
        "history_hash": &state.history_hash,
    });
    Ok((source, state))
}

fn apply_document_param(
    params: &JsonValue,
    require_expected_root: bool,
) -> MethodResult<JsonValue> {
    let object = params_object(params)?;
    let mut apply_document = if let Some(apply) = object.get("apply") {
        if !apply.is_object() {
            return Err(WorkspaceMethodError::invalid_params(
                "ops apply/preview field apply must be a JSON object",
            ));
        }
        apply.clone()
    } else if object.get("schema").and_then(JsonValue::as_str) == Some(WORKSPACE_TRANSACTION_SCHEMA)
    {
        let transaction =
            serde_json::from_value::<WorkspaceTransaction>(params.clone()).map_err(|err| {
                WorkspaceMethodError::invalid_params(format!(
                    "workspace transaction must match {WORKSPACE_TRANSACTION_SCHEMA}: {err}"
                ))
            })?;
        if let Some(schema) = &transaction.schema
            && schema != WORKSPACE_TRANSACTION_SCHEMA
        {
            return Err(WorkspaceMethodError::invalid_params(format!(
                "unsupported workspace transaction schema {schema:?}; expected {WORKSPACE_TRANSACTION_SCHEMA}",
            )));
        }
        json!({
            "schema": APPLY_SCHEMA,
            "branch": transaction.branch,
            "expect_root_hash": transaction.expected_root_hash,
            "agent": transaction.agent,
            "operations": transaction.operations,
        })
    } else {
        params.clone()
    };

    normalize_apply_expected_root_aliases(&mut apply_document)?;
    if require_expected_root && expected_root_hash_from_apply_document(&apply_document)?.is_none() {
        return Err(WorkspaceMethodError::invalid_params(
            "ops.apply requires a batch-level expected_root",
        ));
    }
    Ok(apply_document)
}

fn normalize_apply_expected_root_aliases(apply_document: &mut JsonValue) -> MethodResult<()> {
    let object = apply_document
        .as_object_mut()
        .ok_or_else(|| WorkspaceMethodError::invalid_params("apply document must be an object"))?;
    let root_aliases = [
        "expect_root_hash",
        "expected_root_hash",
        "expect_root",
        "expected_root",
    ];
    let mut expected_root: Option<String> = None;
    for key in root_aliases {
        let Some(value) = object.remove(key) else {
            continue;
        };
        let value = value
            .as_str()
            .ok_or_else(|| WorkspaceMethodError::invalid_params(format!("{key} must be a string")))?
            .to_string();
        if let Some(previous) = &expected_root
            && previous != &value
        {
            return Err(WorkspaceMethodError::invalid_params(
                "conflicting expected root aliases",
            ));
        }
        expected_root = Some(value);
    }
    if let Some(expected_root) = expected_root {
        object.insert(
            "expect_root_hash".to_string(),
            JsonValue::String(expected_root),
        );
    }
    Ok(())
}

fn expected_root_hash_from_apply_document(
    apply_document: &JsonValue,
) -> MethodResult<Option<&str>> {
    let object = apply_document
        .as_object()
        .ok_or_else(|| WorkspaceMethodError::invalid_params("apply document must be an object"))?;
    optional_str_any(
        object,
        &[
            "expect_root_hash",
            "expected_root_hash",
            "expect_root",
            "expected_root",
        ],
    )
}

fn apply_result_workspace_error(
    result: &JsonValue,
    snapshot: WorkspaceSnapshot,
) -> Option<WorkspaceMethodError> {
    let status = result.get("status").and_then(JsonValue::as_str)?;
    match status {
        "conflict" => Some(apply_conflict_workspace_error(result, snapshot)),
        "error" => Some(apply_error_workspace_error(result, snapshot)),
        _ => None,
    }
}

fn apply_conflict_workspace_error(
    result: &JsonValue,
    snapshot: WorkspaceSnapshot,
) -> WorkspaceMethodError {
    let conflict = first_apply_result_with_status(result, "conflict").unwrap_or(result);
    let expected_root_hash = conflict
        .get("expected_root_hash")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let actual_root_hash = conflict
        .get("current_root_hash")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .or_else(|| Some(snapshot.root_hash.clone()));
    let kind = classify_apply_conflict(conflict);
    let operation = conflict
        .get("summary")
        .and_then(|summary| summary.get("kind"))
        .and_then(JsonValue::as_str)
        .unwrap_or("operation");
    let display = conflict
        .get("summary")
        .and_then(|summary| summary.get("display"))
        .and_then(JsonValue::as_str)
        .unwrap_or("workspace apply");
    WorkspaceMethodError::new(kind, format!("{operation} conflict: {display}"))
        .with_snapshot(snapshot)
        .with_roots(expected_root_hash, actual_root_hash)
        .with_diagnostics(vec![WorkspaceDiagnostic {
            kind: "apply_conflict".to_string(),
            message: "codedb/apply/v1 returned conflict".to_string(),
            details: Some(result.clone()),
        }])
}

fn apply_error_workspace_error(
    result: &JsonValue,
    snapshot: WorkspaceSnapshot,
) -> WorkspaceMethodError {
    let error_result = first_apply_result_with_status(result, "error").unwrap_or(result);
    let message = error_result
        .get("error")
        .or_else(|| result.get("error"))
        .and_then(JsonValue::as_str)
        .unwrap_or("ops.apply failed");
    let kind = if message.to_ascii_lowercase().contains("type") {
        "type_error"
    } else {
        "invalid_operation"
    };
    WorkspaceMethodError::new(kind, message.to_string())
        .with_snapshot(snapshot)
        .with_diagnostics(vec![WorkspaceDiagnostic {
            kind: "apply_error".to_string(),
            message: "codedb/apply/v1 returned error".to_string(),
            details: Some(result.clone()),
        }])
}

fn first_apply_result_with_status<'a>(
    result: &'a JsonValue,
    status: &str,
) -> Option<&'a JsonValue> {
    result
        .get("results")
        .and_then(JsonValue::as_array)?
        .iter()
        .find(|entry| entry.get("status").and_then(JsonValue::as_str) == Some(status))
}

fn classify_apply_conflict(conflict: &JsonValue) -> &'static str {
    let failed_preconditions = string_array_field(conflict, "failed_preconditions");
    let failed_postconditions = string_array_field(conflict, "failed_postconditions");
    let operation_kind = conflict
        .get("summary")
        .and_then(|summary| summary.get("kind"))
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if failed_preconditions.contains(&"root_is_current") {
        return "stale_root";
    }
    if failed_preconditions
        .iter()
        .chain(failed_postconditions.iter())
        .any(|condition| condition.contains("export"))
    {
        return "export_conflict";
    }
    if operation_kind == "change_function_signature"
        || failed_postconditions.contains(&"signature_source_matches")
    {
        return "signature_conflict";
    }
    if operation_kind == "delete_symbol" || failed_postconditions.contains(&"symbol_absent") {
        return "delete_conflict";
    }
    if failed_preconditions
        .iter()
        .chain(failed_postconditions.iter())
        .any(|condition| condition.contains("name") || condition.contains("alias"))
    {
        return "name_conflict";
    }
    "dependency_conflict"
}

fn string_array_field<'a>(value: &'a JsonValue, key: &str) -> Vec<&'a str> {
    value
        .get(key)
        .and_then(JsonValue::as_array)
        .map(|values| values.iter().filter_map(JsonValue::as_str).collect())
        .unwrap_or_default()
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

fn optional_str_any<'a>(
    object: &'a JsonMap<String, JsonValue>,
    keys: &[&str],
) -> MethodResult<Option<&'a str>> {
    for key in keys {
        if let Some(value) = optional_str(object, key)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn optional_string_array(
    object: &JsonMap<String, JsonValue>,
    key: &str,
) -> MethodResult<Option<Vec<String>>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let array = value
        .as_array()
        .ok_or_else(|| WorkspaceMethodError::invalid_params(format!("{key} must be an array")))?;
    let mut out = Vec::with_capacity(array.len());
    for (idx, value) in array.iter().enumerate() {
        out.push(
            value
                .as_str()
                .ok_or_else(|| {
                    WorkspaceMethodError::invalid_params(format!("{key}[{idx}] must be a string"))
                })?
                .to_string(),
        );
    }
    Ok(Some(out))
}

fn required_str_any<'a>(
    object: &'a JsonMap<String, JsonValue>,
    keys: &[&str],
) -> MethodResult<&'a str> {
    optional_str_any(object, keys)?.ok_or_else(|| {
        WorkspaceMethodError::invalid_params(format!("method requires one of {}", keys.join(", ")))
    })
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

fn trace_workspace_diagnostics(result: &JsonValue) -> Vec<WorkspaceDiagnostic> {
    result
        .get("diagnostics")
        .and_then(JsonValue::as_array)
        .map(|diagnostics| {
            diagnostics
                .iter()
                .map(|diagnostic| WorkspaceDiagnostic {
                    kind: diagnostic
                        .get("kind")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("trace_diagnostic")
                        .to_string(),
                    message: diagnostic
                        .get("message")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("trace diagnostic")
                        .to_string(),
                    details: Some(diagnostic.clone()),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn nested_result_workspace_error(
    kind: &'static str,
    result: &JsonValue,
    mut diagnostics: Vec<WorkspaceDiagnostic>,
    snapshot: WorkspaceSnapshot,
) -> WorkspaceMethodError {
    let message = diagnostics
        .first()
        .map(|diagnostic| diagnostic.message.clone())
        .or_else(|| first_debug_command_error_message(result))
        .unwrap_or_else(|| format!("{kind} returned an error result"));
    if diagnostics.is_empty() {
        diagnostics.push(WorkspaceDiagnostic {
            kind: kind.to_string(),
            message: message.clone(),
            details: Some(result.clone()),
        });
    }
    WorkspaceMethodError::new(kind, message)
        .with_snapshot(snapshot)
        .with_diagnostics(diagnostics)
}

fn first_debug_command_error_message(result: &JsonValue) -> Option<String> {
    result
        .get("commands")
        .and_then(JsonValue::as_array)?
        .iter()
        .find(|command| command.get("status").and_then(JsonValue::as_str) == Some("error"))
        .and_then(|command| command.get("message"))
        .and_then(JsonValue::as_str)
        .map(str::to_string)
}
