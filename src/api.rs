use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

use crate::MAIN_BRANCH;
use crate::expr::RawExpr;
use crate::migrations::{MigrationStatus, Operation};
use crate::store::{CodeDb, canonical_json};
use crate::types::ParamSpec;

const APPLY_SCHEMA: &str = "codedb/apply/v1";
const APPLY_RESULT_SCHEMA: &str = "codedb/apply-result/v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyBatch {
    #[serde(default)]
    schema: Option<String>,
    #[serde(default = "default_branch")]
    branch: String,
    #[serde(default, alias = "expect_root")]
    expect_root_hash: Option<String>,
    operations: Vec<ApiOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ApiOperation {
    CreateFunction {
        #[serde(default = "default_module")]
        module: String,
        name: String,
        #[serde(default)]
        birth_seed: Option<String>,
        params: Vec<ParamSpec>,
        return_type: String,
        body: RawExpr,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
    RenameSymbol {
        #[serde(default = "default_module")]
        module: String,
        #[serde(default)]
        symbol: Option<String>,
        #[serde(alias = "old_name")]
        name: String,
        new_name: String,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
    ReplaceFunctionBody {
        #[serde(default = "default_module")]
        module: String,
        #[serde(default)]
        symbol: Option<String>,
        name: String,
        body: RawExpr,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
    ChangeFunctionSignature {
        #[serde(default = "default_module")]
        module: String,
        #[serde(default)]
        symbol: Option<String>,
        name: String,
        params: Vec<ParamSpec>,
        return_type: String,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
    DeleteSymbol {
        #[serde(default = "default_module")]
        module: String,
        #[serde(default)]
        symbol: Option<String>,
        name: String,
        #[serde(default)]
        force: bool,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
    CreateAlias {
        #[serde(default = "default_module")]
        module: String,
        #[serde(default)]
        symbol: Option<String>,
        name: String,
        alias: String,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
    RemoveAlias {
        #[serde(default = "default_module")]
        module: String,
        #[serde(default)]
        symbol: Option<String>,
        name: String,
        alias: String,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
    SetExport {
        #[serde(default = "default_module")]
        module: String,
        #[serde(default)]
        symbol: Option<String>,
        name: String,
        exported_name: String,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
    RemoveExport {
        #[serde(default = "default_module")]
        module: String,
        #[serde(default)]
        symbol: Option<String>,
        name: String,
        exported_name: String,
        #[serde(default, alias = "expect_root")]
        expect_root_hash: Option<String>,
    },
}

impl ApiOperation {
    fn expect_root_hash(&self) -> Option<&str> {
        match self {
            ApiOperation::CreateFunction {
                expect_root_hash, ..
            }
            | ApiOperation::RenameSymbol {
                expect_root_hash, ..
            }
            | ApiOperation::ReplaceFunctionBody {
                expect_root_hash, ..
            }
            | ApiOperation::ChangeFunctionSignature {
                expect_root_hash, ..
            }
            | ApiOperation::DeleteSymbol {
                expect_root_hash, ..
            }
            | ApiOperation::CreateAlias {
                expect_root_hash, ..
            }
            | ApiOperation::RemoveAlias {
                expect_root_hash, ..
            }
            | ApiOperation::SetExport {
                expect_root_hash, ..
            }
            | ApiOperation::RemoveExport {
                expect_root_hash, ..
            } => expect_root_hash.as_deref(),
        }
    }
}

impl CodeDb {
    pub fn apply_json_file(&mut self, path: &Path) -> Result<String> {
        self.ensure_initialized()?;
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        self.apply_json_str(&text)
            .with_context(|| format!("failed to apply {}", path.display()))
    }

    pub fn apply_json_str(&mut self, text: &str) -> Result<String> {
        self.ensure_initialized()?;
        let request = parse_apply_json_request(text)?;
        self.apply_batch(request)
    }

    pub fn preview_apply_json_str(&mut self, text: &str) -> Result<String> {
        self.ensure_initialized()?;
        let request = parse_apply_json_request(text)?;
        self.preview_apply_batch(request)
    }

    fn apply_batch(&mut self, request: ApplyBatch) -> Result<String> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.apply_batch_in_tx(request);
        match result {
            Ok((json, should_commit)) => {
                if should_commit {
                    self.conn.execute_batch("COMMIT")?;
                } else {
                    self.conn.execute_batch("ROLLBACK")?;
                }
                Ok(json)
            }
            Err(err) => {
                if let Err(rollback_err) = self.conn.execute_batch("ROLLBACK") {
                    return Err(err).context(format!("rollback failed: {rollback_err}"));
                }
                Err(err)
            }
        }
    }

    fn preview_apply_batch(&mut self, request: ApplyBatch) -> Result<String> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.apply_batch_in_tx(request);
        match result {
            Ok((json, would_commit)) => {
                self.conn.execute_batch("ROLLBACK")?;
                preview_apply_result_json(&json, would_commit)
            }
            Err(err) => {
                if let Err(rollback_err) = self.conn.execute_batch("ROLLBACK") {
                    return Err(err).context(format!("rollback failed: {rollback_err}"));
                }
                Err(err)
            }
        }
    }

    fn apply_batch_in_tx(&mut self, request: ApplyBatch) -> Result<(String, bool)> {
        if let Some(schema) = &request.schema
            && schema != APPLY_SCHEMA
        {
            bail!("unsupported apply schema {schema:?}; expected {APPLY_SCHEMA}");
        }
        if request.branch != MAIN_BRANCH {
            bail!(
                "only branch {MAIN_BRANCH:?} is supported by apply, got {:?}",
                request.branch
            );
        }

        let initial_branch = self.branch(MAIN_BRANCH)?;
        let initial_root = initial_branch.root_hash;
        let initial_history = initial_branch.history_hash;
        let mut results = Vec::new();
        let mut tentative_applied_operation_count = 0usize;
        let mut aggregate_status = MigrationStatus::AlreadyApplied;

        for (idx, api_operation) in request.operations.iter().enumerate() {
            let branch = self.branch(MAIN_BRANCH)?;
            let savepoint = format!("codedb_apply_operation_{idx}");
            self.conn.execute_batch(&format!("SAVEPOINT {savepoint}"))?;
            let expected_root = api_operation
                .expect_root_hash()
                .map(str::to_string)
                .or_else(|| {
                    if idx == 0 {
                        request.expect_root_hash.clone()
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| branch.root_hash.clone());
            let operation = match self.operation_from_api(&expected_root, api_operation) {
                Ok(operation) => operation,
                Err(err) => {
                    self.conn.execute_batch(&format!(
                        "ROLLBACK TO SAVEPOINT {savepoint}; RELEASE SAVEPOINT {savepoint}"
                    ))?;
                    let mut results = mark_rolled_back_results(results);
                    let message = format!("operation {idx} is invalid: {err:#}");
                    results.push(apply_error_result_json(
                        idx,
                        &branch.root_hash,
                        &message,
                        None,
                    ));
                    let payload = apply_result_json(
                        "error",
                        false,
                        Some("error"),
                        &initial_root,
                        &initial_root,
                        initial_history.as_ref(),
                        request.operations.len(),
                        results.len(),
                        0,
                        Some(&message),
                        results,
                    );
                    return Ok((format!("{}\n", canonical_json(&payload)), false));
                }
            };
            let summary = self.migration_summary(&operation);
            let (outcome, operation_changed_root) =
                match self.apply_and_record_expected_in_tx(&expected_root, operation) {
                    Ok(result) => result,
                    Err(err) => {
                        self.conn.execute_batch(&format!(
                            "ROLLBACK TO SAVEPOINT {savepoint}; RELEASE SAVEPOINT {savepoint}"
                        ))?;
                        let mut results = mark_rolled_back_results(results);
                        let message = format!("operation {idx} failed: {err:#}");
                        results.push(apply_error_result_json(
                            idx,
                            &branch.root_hash,
                            &message,
                            Some(&summary),
                        ));
                        let payload = apply_result_json(
                            "error",
                            false,
                            Some("error"),
                            &initial_root,
                            &initial_root,
                            initial_history.as_ref(),
                            request.operations.len(),
                            results.len(),
                            0,
                            Some(&message),
                            results,
                        );
                        return Ok((format!("{}\n", canonical_json(&payload)), false));
                    }
                };
            let status = outcome.status();
            if matches!(status, MigrationStatus::Applied) {
                debug_assert!(operation_changed_root);
                tentative_applied_operation_count += 1;
                self.conn
                    .execute_batch(&format!("RELEASE SAVEPOINT {savepoint}"))?;
            } else {
                self.conn.execute_batch(&format!(
                    "ROLLBACK TO SAVEPOINT {savepoint}; RELEASE SAVEPOINT {savepoint}"
                ))?;
            }
            aggregate_status = merge_status(aggregate_status, status);
            let stop = matches!(status, MigrationStatus::Conflict);
            results.push(outcome.to_json());
            if stop {
                let results = mark_rolled_back_results(results);
                let payload = apply_result_json(
                    aggregate_status.as_str(),
                    false,
                    Some("conflict"),
                    &initial_root,
                    &initial_root,
                    initial_history.as_ref(),
                    request.operations.len(),
                    results.len(),
                    0,
                    None,
                    results,
                );
                return Ok((format!("{}\n", canonical_json(&payload)), false));
            }
        }

        let should_commit = tentative_applied_operation_count > 0;
        let (new_root, history_hash, applied_operation_count) = if should_commit {
            let final_branch = self.branch(MAIN_BRANCH)?;
            (
                final_branch.root_hash,
                final_branch.history_hash,
                tentative_applied_operation_count,
            )
        } else {
            (initial_root.clone(), initial_history.clone(), 0)
        };
        let payload = apply_result_json(
            aggregate_status.as_str(),
            should_commit,
            None,
            &initial_root,
            &new_root,
            history_hash.as_ref(),
            request.operations.len(),
            results.len(),
            applied_operation_count,
            None,
            results,
        );
        Ok((format!("{}\n", canonical_json(&payload)), should_commit))
    }

    fn operation_from_api(
        &self,
        expected_root: &str,
        api_operation: &ApiOperation,
    ) -> Result<Operation> {
        match api_operation {
            ApiOperation::CreateFunction {
                module,
                name,
                birth_seed,
                params,
                return_type,
                body,
                ..
            } => Ok(Operation::CreateFunction {
                module: module.clone(),
                name: name.clone(),
                birth_seed: birth_seed
                    .clone()
                    .unwrap_or_else(|| format!("json:{module}:{name}")),
                params: params.clone(),
                return_type: return_type.clone(),
                body: body.clone(),
            }),
            ApiOperation::RenameSymbol {
                module,
                symbol,
                name,
                new_name,
                ..
            } => {
                let symbol = match symbol {
                    Some(symbol) => symbol.clone(),
                    None => match self.resolve_name(expected_root, module, name) {
                        Ok(symbol) => symbol,
                        Err(err) => self
                            .resolve_name(expected_root, module, new_name)
                            .map_err(|_| err)?,
                    },
                };
                Ok(Operation::RenameSymbol {
                    module: module.clone(),
                    symbol,
                    old_name: name.clone(),
                    new_name: new_name.clone(),
                })
            }
            ApiOperation::ReplaceFunctionBody {
                module,
                symbol,
                name,
                body,
                ..
            } => Ok(Operation::ReplaceFunctionBody {
                module: module.clone(),
                symbol: self.symbol_or_resolve(expected_root, module, name, symbol)?,
                name: name.clone(),
                body: body.clone(),
            }),
            ApiOperation::ChangeFunctionSignature {
                module,
                symbol,
                name,
                params,
                return_type,
                ..
            } => Ok(Operation::ChangeFunctionSignature {
                module: module.clone(),
                symbol: self.symbol_or_resolve(expected_root, module, name, symbol)?,
                name: name.clone(),
                params: params.clone(),
                return_type: return_type.clone(),
            }),
            ApiOperation::DeleteSymbol {
                module,
                symbol,
                name,
                force,
                ..
            } => Ok(Operation::DeleteSymbol {
                module: module.clone(),
                symbol: self.symbol_or_resolve(expected_root, module, name, symbol)?,
                name: name.clone(),
                force: *force,
            }),
            ApiOperation::CreateAlias {
                module,
                symbol,
                name,
                alias,
                ..
            } => Ok(Operation::CreateAlias {
                module: module.clone(),
                symbol: self.symbol_or_resolve(expected_root, module, name, symbol)?,
                name: name.clone(),
                alias: alias.clone(),
            }),
            ApiOperation::RemoveAlias {
                module,
                symbol,
                name,
                alias,
                ..
            } => Ok(Operation::RemoveAlias {
                module: module.clone(),
                symbol: self.symbol_or_resolve(expected_root, module, name, symbol)?,
                name: name.clone(),
                alias: alias.clone(),
            }),
            ApiOperation::SetExport {
                module,
                symbol,
                name,
                exported_name,
                ..
            } => Ok(Operation::SetExport {
                module: module.clone(),
                symbol: self.symbol_or_resolve(expected_root, module, name, symbol)?,
                name: name.clone(),
                exported_name: exported_name.clone(),
            }),
            ApiOperation::RemoveExport {
                module,
                symbol,
                name,
                exported_name,
                ..
            } => Ok(Operation::RemoveExport {
                module: module.clone(),
                symbol: self.symbol_or_resolve(expected_root, module, name, symbol)?,
                name: name.clone(),
                exported_name: exported_name.clone(),
            }),
        }
    }

    fn symbol_or_resolve(
        &self,
        root_hash: &str,
        module: &str,
        name: &str,
        symbol: &Option<String>,
    ) -> Result<String> {
        symbol
            .clone()
            .map(Ok)
            .unwrap_or_else(|| self.resolve_name(root_hash, module, name))
            .with_context(|| anyhow!("failed to resolve {module}.{name}"))
    }
}

fn parse_apply_json_request(text: &str) -> Result<ApplyBatch> {
    let value: JsonValue =
        serde_json::from_str(text).context("apply JSON must be a JSON object")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("apply JSON must be an object"))?;
    let request = if object.contains_key("operations") {
        serde_json::from_value::<ApplyBatch>(value)
            .context("apply JSON must match codedb/apply/v1")?
    } else {
        let mut operation_value = value;
        if let JsonValue::Object(map) = &mut operation_value
            && let Some(schema) = map.remove("schema")
        {
            let schema = schema
                .as_str()
                .ok_or_else(|| anyhow!("apply schema must be {APPLY_SCHEMA:?}"))?;
            if schema != APPLY_SCHEMA {
                bail!("unsupported apply schema {schema:?}; expected {APPLY_SCHEMA}");
            }
        }
        let operation = serde_json::from_value::<ApiOperation>(operation_value)
            .context("apply JSON must match codedb/apply/v1 operation schema")?;
        ApplyBatch {
            schema: Some(APPLY_SCHEMA.to_string()),
            branch: MAIN_BRANCH.to_string(),
            expect_root_hash: operation.expect_root_hash().map(str::to_string),
            operations: vec![operation],
        }
    };
    Ok(request)
}

fn preview_apply_result_json(text: &str, would_commit: bool) -> Result<String> {
    let mut payload: JsonValue =
        serde_json::from_str(text.trim_end()).context("apply preview result must be JSON")?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("apply preview result must be a JSON object"))?;
    object.insert("preview".to_string(), JsonValue::Bool(true));
    object.insert("would_commit".to_string(), JsonValue::Bool(would_commit));
    object.insert("committed".to_string(), JsonValue::Bool(false));
    if would_commit {
        object.insert(
            "rollback_reason".to_string(),
            JsonValue::String("preview".to_string()),
        );
    }
    Ok(format!("{}\n", canonical_json(&payload)))
}

#[allow(clippy::too_many_arguments)]
fn apply_result_json(
    status: &str,
    committed: bool,
    rollback_reason: Option<&str>,
    old_root_hash: &str,
    new_root_hash: &str,
    history_hash: Option<&String>,
    operation_count: usize,
    processed_operation_count: usize,
    applied_operation_count: usize,
    error: Option<&str>,
    results: Vec<serde_json::Value>,
) -> serde_json::Value {
    json!({
        "schema": APPLY_RESULT_SCHEMA,
        "status": status,
        "branch": MAIN_BRANCH,
        "atomic": true,
        "committed": committed,
        "rollback_reason": rollback_reason,
        "error": error,
        "old_root_hash": old_root_hash,
        "new_root_hash": new_root_hash,
        "history_hash": history_hash,
        "operation_count": operation_count,
        "processed_operation_count": processed_operation_count,
        "applied_operation_count": applied_operation_count,
        "results": results,
    })
}

fn apply_error_result_json(
    operation_index: usize,
    current_root_hash: &str,
    error: &str,
    summary: Option<&crate::migrations::MigrationSummary>,
) -> JsonValue {
    json!({
        "status": "error",
        "operation_index": operation_index,
        "current_root_hash": current_root_hash,
        "migration_hash": JsonValue::Null,
        "history_hash": JsonValue::Null,
        "rolled_back": true,
        "error": error,
        "summary": summary.map(crate::migrations::MigrationSummary::to_json),
    })
}

fn mark_rolled_back_results(results: Vec<JsonValue>) -> Vec<JsonValue> {
    results
        .into_iter()
        .map(|mut result| {
            if result.get("status").and_then(JsonValue::as_str) == Some("applied")
                && let Some(object) = result.as_object_mut()
            {
                object.insert(
                    "status".to_string(),
                    JsonValue::String("rolled_back".to_string()),
                );
                object.insert("migration_hash".to_string(), JsonValue::Null);
                object.insert("history_hash".to_string(), JsonValue::Null);
                object.insert("rolled_back".to_string(), JsonValue::Bool(true));
            }
            result
        })
        .collect()
}

fn merge_status(current: MigrationStatus, next: MigrationStatus) -> MigrationStatus {
    match (current, next) {
        (MigrationStatus::Conflict, _) | (_, MigrationStatus::Conflict) => {
            MigrationStatus::Conflict
        }
        (MigrationStatus::Applied, _) | (_, MigrationStatus::Applied) => MigrationStatus::Applied,
        (MigrationStatus::AlreadyApplied, MigrationStatus::AlreadyApplied) => {
            MigrationStatus::AlreadyApplied
        }
    }
}

fn default_branch() -> String {
    MAIN_BRANCH.to_string()
}

fn default_module() -> String {
    "main".to_string()
}
