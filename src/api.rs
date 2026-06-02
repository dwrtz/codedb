use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;

use crate::MAIN_BRANCH;
use crate::expr::RawExpr;
use crate::migrations::{MigrationStatus, Operation};
use crate::store::{CodeDb, canonical_json};
use crate::types::ParamSpec;

const APPLY_SCHEMA: &str = "codedb/apply/v1";
const APPLY_RESULT_SCHEMA: &str = "codedb/apply-result/v1";

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ApplyDocument {
    Batch(ApplyBatch),
    Operation(ApiOperation),
}

#[derive(Debug, Deserialize)]
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
#[serde(tag = "kind", rename_all = "snake_case")]
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
        let document: ApplyDocument =
            serde_json::from_str(text).context("apply JSON must match codedb/apply/v1")?;
        let request = match document {
            ApplyDocument::Batch(batch) => batch,
            ApplyDocument::Operation(operation) => ApplyBatch {
                schema: Some(APPLY_SCHEMA.to_string()),
                branch: MAIN_BRANCH.to_string(),
                expect_root_hash: operation.expect_root_hash().map(str::to_string),
                operations: vec![operation],
            },
        };
        self.apply_batch(request)
    }

    fn apply_batch(&mut self, request: ApplyBatch) -> Result<String> {
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
        let mut results = Vec::new();
        let mut applied_operation_count = 0usize;
        let mut aggregate_status = MigrationStatus::AlreadyApplied;

        for (idx, api_operation) in request.operations.iter().enumerate() {
            let branch = self.branch(MAIN_BRANCH)?;
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
            let operation = self
                .operation_from_api(&expected_root, api_operation)
                .with_context(|| format!("operation {idx} is invalid"))?;
            let outcome = self.apply_and_record_expected(branch, &expected_root, operation)?;
            let status = outcome.status();
            if matches!(status, MigrationStatus::Applied) {
                applied_operation_count += 1;
            }
            aggregate_status = merge_status(aggregate_status, status);
            let stop = matches!(status, MigrationStatus::Conflict);
            results.push(outcome.to_json());
            if stop {
                break;
            }
        }

        let final_branch = self.branch(MAIN_BRANCH)?;
        let payload = json!({
            "schema": APPLY_RESULT_SCHEMA,
            "status": aggregate_status.as_str(),
            "branch": MAIN_BRANCH,
            "old_root_hash": initial_root,
            "new_root_hash": final_branch.root_hash,
            "history_hash": final_branch.history_hash,
            "operation_count": request.operations.len(),
            "processed_operation_count": results.len(),
            "applied_operation_count": applied_operation_count,
            "results": results,
        });
        Ok(format!("{}\n", canonical_json(&payload)))
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
