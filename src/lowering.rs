use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::artifact::CacheKeyInput;
use crate::backend::ArtifactKind;
use crate::model::{ProgramRootPayload, RootSymbolPayload};
use crate::store::{CodeDb, canonical_json, hash_bytes};
use crate::types::type_hash_for;
use crate::{BYTES_DOMAIN, MAIN_BRANCH};

pub(crate) const LOWERED_IR_SCHEMA: &str = "codedb/lowered-function-ir/v1";
pub(crate) const LOWERED_DEBUG_MAP_SCHEMA: &str = "codedb/lowered-debug-map/v1";
const LOWERED_IR_INSPECTION_SCHEMA: &str = "codedb/lowered-ir-inspection/v1";
const LOWERING_BACKEND_ID: &str = "lowering-v0";
const LOWERING_TARGET: &str = "target-independent-ir-v0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredFunctionIr {
    pub(crate) schema: String,
    pub(crate) symbol_hash: String,
    pub(crate) function_def_hash: String,
    pub(crate) function_sig_hash: String,
    pub(crate) typed_body_expr_hash: String,
    pub(crate) params: Vec<LoweredParamSlot>,
    pub(crate) return_type_hash: String,
    pub(crate) operations: Vec<LoweredOp>,
    #[serde(default)]
    pub(crate) debug_map: LoweredDebugMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredParamSlot {
    pub(crate) slot: usize,
    pub(crate) type_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredBlock {
    pub(crate) operations: Vec<LoweredOp>,
    pub(crate) result: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredDebugMap {
    pub(crate) schema: String,
    pub(crate) operations: Vec<LoweredDebugOp>,
    pub(crate) expr_to_ops: Vec<LoweredExprOpMap>,
}

impl Default for LoweredDebugMap {
    fn default() -> Self {
        Self {
            schema: LOWERED_DEBUG_MAP_SCHEMA.to_string(),
            operations: Vec::new(),
            expr_to_ops: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredDebugOp {
    pub(crate) lowered_op_id: String,
    pub(crate) value_id: String,
    pub(crate) lowered_op_kind: String,
    pub(crate) expr_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredExprOpMap {
    pub(crate) expr_hash: String,
    pub(crate) lowered_op_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredTrap {
    pub(crate) condition: String,
    pub(crate) code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum LoweredOp {
    Param {
        id: String,
        slot: usize,
        type_hash: String,
    },
    ConstI64 {
        id: String,
        value: String,
        type_hash: String,
    },
    ConstBool {
        id: String,
        value: bool,
        type_hash: String,
    },
    ConstUnit {
        id: String,
        type_hash: String,
    },
    Unary {
        id: String,
        kind: String,
        value: String,
        type_hash: String,
    },
    Binary {
        id: String,
        kind: String,
        left: String,
        right: String,
        type_hash: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        trap: Option<LoweredTrap>,
    },
    Call {
        id: String,
        target_symbol_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_abi_symbol: Option<String>,
        args: Vec<String>,
        type_hash: String,
    },
    If {
        id: String,
        cond: String,
        then_block: LoweredBlock,
        else_block: LoweredBlock,
        type_hash: String,
    },
    Return {
        value: String,
        type_hash: String,
    },
}

pub(crate) struct LoweredFunctionArtifact {
    pub(crate) ir: LoweredFunctionIr,
    pub(crate) lowered_ir_hash: String,
}

struct LoweredExpr {
    operations: Vec<LoweredOp>,
    value: String,
    type_hash: String,
}

#[derive(Debug, Clone)]
struct LocalLoweredBinding {
    value: String,
    type_hash: String,
}

#[derive(Default)]
struct LowerCtx {
    next_value: usize,
    debug_operations: Vec<LoweredDebugOp>,
}

impl LowerCtx {
    fn value(&mut self) -> String {
        let value = format!("v{}", self.next_value);
        self.next_value += 1;
        value
    }

    fn push_debug_op(&mut self, expr_hash: &str, lowered_op_kind: &str, value_id: &str) {
        self.debug_operations.push(LoweredDebugOp {
            lowered_op_id: lowered_op_id_for_value(value_id),
            value_id: value_id.to_string(),
            lowered_op_kind: lowered_op_kind.to_string(),
            expr_hash: expr_hash.to_string(),
        });
    }

    fn into_debug_map(self) -> LoweredDebugMap {
        let mut by_expr: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for op in &self.debug_operations {
            by_expr
                .entry(op.expr_hash.clone())
                .or_default()
                .push(op.lowered_op_id.clone());
        }
        LoweredDebugMap {
            schema: LOWERED_DEBUG_MAP_SCHEMA.to_string(),
            operations: self.debug_operations,
            expr_to_ops: by_expr
                .into_iter()
                .map(|(expr_hash, lowered_op_ids)| LoweredExprOpMap {
                    expr_hash,
                    lowered_op_ids,
                })
                .collect(),
        }
    }
}

impl CodeDb {
    pub fn emit_ir_main_branch(&mut self, function_name: &str) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self
            .resolve_symbol_or_name(&branch.root_hash, function_name)
            .map_err(|err| anyhow!("unknown entry function {function_name}: {err}"))?;
        let artifact = self.lower_symbol(&branch.root_hash, &symbol)?;
        let inspection = json!({
            "schema": LOWERED_IR_INSPECTION_SCHEMA,
            "lowered_ir_hash": artifact.lowered_ir_hash,
            "ir": artifact.ir,
        });
        Ok(format!("{}\n", serde_json::to_string_pretty(&inspection)?))
    }

    pub(crate) fn lower_symbol(
        &mut self,
        root_hash: &str,
        symbol: &str,
    ) -> Result<LoweredFunctionArtifact> {
        let root = self.load_root(root_hash)?;
        let entry = self
            .root_symbol(&root, symbol)
            .cloned()
            .ok_or_else(|| anyhow!("missing symbol {symbol}"))?;
        self.lower_function_definition(&root, &entry)
    }

    fn lower_function_definition(
        &mut self,
        root: &ProgramRootPayload,
        entry: &RootSymbolPayload,
    ) -> Result<LoweredFunctionArtifact> {
        let key_input = CacheKeyInput::new(
            ArtifactKind::LoweredIr,
            &entry.definition,
            LOWERING_BACKEND_ID,
            LOWERING_TARGET,
        );
        if let Some(cache_entry) = self.lookup_cache(&key_input)? {
            let artifact_json = cache_entry
                .artifact_json
                .as_ref()
                .ok_or_else(|| anyhow!("lowered IR cache entry missing artifact_json"))?;
            let ir = lowered_ir_from_artifact_metadata(artifact_json)?;
            self.verify_lowered_ir(root, &ir)?;
            let expected = self.build_lowered_function_ir(root, entry)?;
            let ir_json = serde_json::to_value(&ir)?;
            let recomputed_hash = hash_lowered_ir_json(&ir_json);
            if ir != expected || recomputed_hash != cache_entry.artifact_hash {
                return self.write_lowered_ir_artifact(entry, expected);
            }
            return Ok(LoweredFunctionArtifact {
                ir,
                lowered_ir_hash: recomputed_hash,
            });
        }

        let ir = self.build_lowered_function_ir(root, entry)?;
        self.verify_lowered_ir(root, &ir)?;
        self.write_lowered_ir_artifact(entry, ir)
    }

    fn write_lowered_ir_artifact(
        &mut self,
        entry: &RootSymbolPayload,
        ir: LoweredFunctionIr,
    ) -> Result<LoweredFunctionArtifact> {
        let ir_json = serde_json::to_value(&ir)?;
        let lowered_ir_hash = hash_lowered_ir_json(&ir_json);
        self.write_cache_json(
            &entry.definition,
            LOWERING_BACKEND_ID,
            LOWERING_TARGET,
            ArtifactKind::LoweredIr,
            &ir_json,
        )?;
        Ok(LoweredFunctionArtifact {
            ir,
            lowered_ir_hash,
        })
    }

    pub(crate) fn build_lowered_function_ir(
        &self,
        root: &ProgramRootPayload,
        entry: &RootSymbolPayload,
    ) -> Result<LoweredFunctionIr> {
        if !self.definition_is_internal_function(&entry.definition)? {
            bail!("lowering input is not a FunctionDef {}", entry.definition);
        }
        let definition = self.get_payload(&entry.definition)?;
        let symbol = definition
            .get("symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("function definition missing symbol"))?;
        if symbol != entry.symbol {
            bail!(
                "function definition symbol {} does not match root symbol {}",
                symbol,
                entry.symbol
            );
        }
        let signature = self.function_signature_hash(&entry.definition)?;
        if signature != entry.signature {
            bail!(
                "function definition signature {} does not match root signature {}",
                signature,
                entry.signature
            );
        }
        let (param_types, return_type) = self.signature_parts(&entry.signature)?;
        for type_hash in param_types.iter().chain(std::iter::once(&return_type)) {
            self.ensure_lowerable_v0_type(type_hash)?;
        }
        let body = self.function_body_hash(&entry.definition)?;
        let actual_return = self.verify_expr_type(&body, root, &param_types)?;
        if actual_return != return_type {
            bail!(
                "function body type {} does not match return type {}",
                actual_return,
                return_type
            );
        }

        let mut ctx = LowerCtx::default();
        let mut lowered = self.lower_expr(root, &body, &param_types, &mut ctx, &mut Vec::new())?;
        let debug_map = ctx.into_debug_map();
        lowered.operations.push(LoweredOp::Return {
            value: lowered.value,
            type_hash: return_type.clone(),
        });

        Ok(LoweredFunctionIr {
            schema: LOWERED_IR_SCHEMA.to_string(),
            symbol_hash: entry.symbol.clone(),
            function_def_hash: entry.definition.clone(),
            function_sig_hash: entry.signature.clone(),
            typed_body_expr_hash: body,
            params: param_types
                .iter()
                .enumerate()
                .map(|(slot, type_hash)| LoweredParamSlot {
                    slot,
                    type_hash: type_hash.clone(),
                })
                .collect(),
            return_type_hash: return_type,
            operations: lowered.operations,
            debug_map,
        })
    }

    fn lower_expr(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let payload = self.get_payload(expr_hash)?;
        let type_hash = expr_type(&payload, expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        match expr_kind {
            "literal_i64" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .to_string();
                value.parse::<i64>()?;
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_i64", &id);
                Ok(LoweredExpr {
                    operations: vec![LoweredOp::ConstI64 {
                        id: id.clone(),
                        value,
                        type_hash: type_hash.clone(),
                    }],
                    value: id,
                    type_hash,
                })
            }
            "literal_bool" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?;
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_bool", &id);
                Ok(LoweredExpr {
                    operations: vec![LoweredOp::ConstBool {
                        id: id.clone(),
                        value,
                        type_hash: type_hash.clone(),
                    }],
                    value: id,
                    type_hash,
                })
            }
            "literal_unit" => {
                if type_hash != type_hash_for("Unit") {
                    bail!("literal_unit type mismatch");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_unit", &id);
                Ok(LoweredExpr {
                    operations: vec![LoweredOp::ConstUnit {
                        id: id.clone(),
                        type_hash: type_hash.clone(),
                    }],
                    value: id,
                    type_hash,
                })
            }
            "param_ref" => {
                let slot = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                let expected = param_types
                    .get(slot)
                    .ok_or_else(|| anyhow!("parameter slot out of bounds {slot}"))?;
                if expected != &type_hash {
                    bail!("parameter slot {slot} type mismatch");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "param", &id);
                Ok(LoweredExpr {
                    operations: vec![LoweredOp::Param {
                        id: id.clone(),
                        slot,
                        type_hash: type_hash.clone(),
                    }],
                    value: id,
                    type_hash,
                })
            }
            "local_ref" => {
                let depth = payload
                    .get("depth")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("local_ref missing depth"))?
                    as usize;
                let binding = local_lowered_at_depth(locals, depth)
                    .ok_or_else(|| anyhow!("local_ref depth out of bounds {depth}"))?;
                if binding.type_hash != type_hash {
                    bail!("local_ref type mismatch while lowering");
                }
                Ok(LoweredExpr {
                    operations: Vec::new(),
                    value: binding.value.clone(),
                    type_hash,
                })
            }
            "call" => {
                let target_symbol_hash = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?
                    .to_string();
                let target = self
                    .root_symbol(root, &target_symbol_hash)
                    .ok_or_else(|| anyhow!("call target missing from root {target_symbol_hash}"))?;
                let target_abi_symbol = if self.definition_is_external(&target.definition)? {
                    Some(
                        self.external_function_metadata(&target.definition)?
                            .link_name,
                    )
                } else {
                    None
                };
                if self.root_symbol(root, &target_symbol_hash).is_none() {
                    bail!("call target missing from root {target_symbol_hash}");
                }
                let mut operations = Vec::new();
                let mut arg_values = Vec::new();
                for arg in payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                {
                    let arg_hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    let lowered = self.lower_expr(root, arg_hash, param_types, ctx, locals)?;
                    operations.extend(lowered.operations);
                    arg_values.push(lowered.value);
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "call", &id);
                operations.push(LoweredOp::Call {
                    id: id.clone(),
                    target_symbol_hash,
                    target_abi_symbol,
                    args: arg_values,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "binary" => {
                let left_hash = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right_hash = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                let left = self.lower_expr(root, left_hash, param_types, ctx, locals)?;
                let right = self.lower_expr(root, right_hash, param_types, ctx, locals)?;
                let source_op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?;
                let kind =
                    lower_binary_kind(source_op, &left.type_hash, &right.type_hash, &type_hash)?;
                let trap = trap_for_binary(&kind);
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "binary", &id);
                let mut operations = left.operations;
                operations.extend(right.operations);
                operations.push(LoweredOp::Binary {
                    id: id.clone(),
                    kind,
                    left: left.value,
                    right: right.value,
                    type_hash: type_hash.clone(),
                    trap,
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "unary" => {
                let child_hash = payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?;
                let child = self.lower_expr(root, child_hash, param_types, ctx, locals)?;
                let source_op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing op"))?;
                let kind = lower_unary_kind(source_op, &child.type_hash, &type_hash)?;
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "unary", &id);
                let mut operations = child.operations;
                operations.push(LoweredOp::Unary {
                    id: id.clone(),
                    kind,
                    value: child.value,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "let" => {
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
                let value = self.lower_expr(root, value_hash, param_types, ctx, locals)?;
                if value.type_hash != binding_type {
                    bail!("let binding type mismatch while lowering");
                }
                locals.push(LocalLoweredBinding {
                    value: value.value.clone(),
                    type_hash: binding_type,
                });
                let body = self.lower_expr(root, body_hash, param_types, ctx, locals);
                locals.pop();
                let body = body?;
                let mut operations = value.operations;
                operations.extend(body.operations);
                Ok(LoweredExpr {
                    operations,
                    value: body.value,
                    type_hash: body.type_hash,
                })
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
                let cond = self.lower_expr(root, cond_hash, param_types, ctx, locals)?;
                if cond.type_hash != type_hash_for("Bool") {
                    bail!("if condition must lower to bool");
                }
                let then_expr = self.lower_expr(root, then_hash, param_types, ctx, locals)?;
                let else_expr = self.lower_expr(root, else_hash, param_types, ctx, locals)?;
                if then_expr.type_hash != else_expr.type_hash || then_expr.type_hash != type_hash {
                    bail!("if branch type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "if", &id);
                let mut operations = cond.operations;
                operations.push(LoweredOp::If {
                    id: id.clone(),
                    cond: cond.value,
                    then_block: LoweredBlock {
                        operations: then_expr.operations,
                        result: then_expr.value,
                    },
                    else_block: LoweredBlock {
                        operations: else_expr.operations,
                        result: else_expr.value,
                    },
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "record_literal" | "field_access" | "enum_construct" | "case" => {
                bail!("lowering v0 does not support aggregate expression kind {expr_kind}")
            }
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn ensure_lowerable_v0_type(&self, type_hash: &str) -> Result<()> {
        let type_name = self.type_name(type_hash)?;
        match type_name.as_str() {
            "i64" | "bool" | "unit" => Ok(()),
            _ => bail!("lowering v0 does not support aggregate type {type_name}"),
        }
    }

    pub(crate) fn verify_lowered_ir_against_index(
        &self,
        input_hash: &str,
        ir: &LoweredFunctionIr,
    ) -> Result<()> {
        if ir.function_def_hash != input_hash {
            bail!(
                "lowered IR input mismatch: cache input {input_hash}, IR input {}",
                ir.function_def_hash
            );
        }
        let root_hashes = self
            .conn
            .prepare(
                "SELECT root_hash FROM root_symbols
                 WHERE definition_hash = ?1 AND symbol_hash = ?2
                 ORDER BY root_hash",
            )?
            .query_map(params![input_hash, &ir.symbol_hash], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if root_hashes.is_empty() {
            return self.verify_lowered_ir_shape(ir);
        }

        let mut last_error = None;
        for root_hash in root_hashes {
            let root = self.load_root(&root_hash)?;
            let Some(entry) = self.root_symbol(&root, &ir.symbol_hash) else {
                last_error = Some(anyhow!(
                    "lowered IR symbol {} missing from indexed root {root_hash}",
                    ir.symbol_hash
                ));
                continue;
            };
            match self.build_lowered_function_ir(&root, entry) {
                Ok(expected) if &expected == ir => return Ok(()),
                Ok(_) => {
                    last_error = Some(anyhow!(
                        "lowered IR does not match recomputed semantic DAG for root {root_hash}"
                    ));
                }
                Err(err) => last_error = Some(err),
            }
        }
        if let Some(err) = last_error {
            bail!("{err:#}");
        }
        bail!("lowered IR does not match any indexed root");
    }

    fn verify_lowered_ir(&self, root: &ProgramRootPayload, ir: &LoweredFunctionIr) -> Result<()> {
        self.verify_lowered_ir_shape(ir)?;
        let root_entry = self
            .root_symbol(root, &ir.symbol_hash)
            .ok_or_else(|| anyhow!("lowered IR symbol missing from root {}", ir.symbol_hash))?;
        if root_entry.definition != ir.function_def_hash {
            bail!("lowered IR definition does not match root");
        }
        if root_entry.signature != ir.function_sig_hash {
            bail!("lowered IR signature does not match root");
        }

        let (param_types, return_type) = self.signature_parts(&ir.function_sig_hash)?;
        let actual_return = self.verify_expr_type(&ir.typed_body_expr_hash, root, &param_types)?;
        if actual_return != return_type || actual_return != ir.return_type_hash {
            bail!("lowered IR return type mismatch");
        }
        self.verify_lowered_operations(root, ir, &param_types, &return_type)
    }

    fn verify_lowered_ir_shape(&self, ir: &LoweredFunctionIr) -> Result<()> {
        if ir.schema != LOWERED_IR_SCHEMA {
            bail!("lowered IR schema must be {LOWERED_IR_SCHEMA}");
        }
        if !is_hash(&ir.symbol_hash)
            || !is_hash(&ir.function_def_hash)
            || !is_hash(&ir.function_sig_hash)
            || !is_hash(&ir.typed_body_expr_hash)
            || !is_hash(&ir.return_type_hash)
        {
            bail!("lowered IR contains a non-hash identity field");
        }
        if self.get_kind(&ir.function_def_hash)? != "FunctionDef" {
            bail!("lowered IR function_def_hash is not a FunctionDef");
        }
        let definition = self.get_payload(&ir.function_def_hash)?;
        if definition.get("symbol").and_then(JsonValue::as_str) != Some(&ir.symbol_hash) {
            bail!("lowered IR symbol does not match FunctionDef");
        }
        if definition
            .get("function_sig_hash")
            .and_then(JsonValue::as_str)
            != Some(&ir.function_sig_hash)
        {
            bail!("lowered IR signature does not match FunctionDef");
        }
        if definition
            .get("typed_body_expr_hash")
            .and_then(JsonValue::as_str)
            != Some(&ir.typed_body_expr_hash)
        {
            bail!("lowered IR body does not match FunctionDef");
        }
        let (param_types, return_type) = self.signature_parts(&ir.function_sig_hash)?;
        if ir.return_type_hash != return_type {
            bail!("lowered IR return type does not match signature");
        }
        if ir.params.len() != param_types.len() {
            bail!("lowered IR parameter count mismatch");
        }
        for (slot, (param, expected_type)) in ir.params.iter().zip(param_types.iter()).enumerate() {
            if param.slot != slot || param.type_hash != *expected_type {
                bail!("lowered IR parameter slot {slot} mismatch");
            }
        }
        if ir.operations.is_empty() {
            bail!("lowered IR has no return operation");
        }
        self.verify_lowered_debug_map(ir)?;
        Ok(())
    }

    fn verify_lowered_debug_map(&self, ir: &LoweredFunctionIr) -> Result<()> {
        if ir.debug_map.schema != LOWERED_DEBUG_MAP_SCHEMA {
            bail!("lowered debug map schema must be {LOWERED_DEBUG_MAP_SCHEMA}");
        }
        let expected_ops = lowered_value_debug_infos(&ir.operations)?;
        if expected_ops.is_empty() {
            bail!("lowered debug map has no value-producing operations");
        }
        let mut seen_op_ids = BTreeSet::new();
        let mut seen_value_ids = BTreeSet::new();
        let mut actual_ops = BTreeMap::new();
        let mut actual_by_expr: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for op in &ir.debug_map.operations {
            if op.lowered_op_id != lowered_op_id_for_value(&op.value_id) {
                bail!(
                    "lowered debug map op id does not match value id {}",
                    op.value_id
                );
            }
            if !seen_op_ids.insert(op.lowered_op_id.clone()) {
                bail!("duplicate lowered debug op id {}", op.lowered_op_id);
            }
            if !seen_value_ids.insert(op.value_id.clone()) {
                bail!("duplicate lowered debug value id {}", op.value_id);
            }
            if !is_hash(&op.expr_hash) {
                bail!("lowered debug map expression is not a hash");
            }
            if self.get_kind(&op.expr_hash)? != "Expression" {
                bail!("lowered debug map expression is not an Expression");
            }
            actual_by_expr
                .entry(op.expr_hash.clone())
                .or_default()
                .push(op.lowered_op_id.clone());
            actual_ops.insert(
                op.value_id.clone(),
                (op.lowered_op_kind.clone(), op.lowered_op_id.clone()),
            );
        }

        let expected_value_ids = expected_ops.keys().cloned().collect::<BTreeSet<_>>();
        if expected_value_ids != seen_value_ids {
            bail!("lowered debug map does not cover all value-producing operations");
        }
        for (value_id, expected_kind) in expected_ops {
            match actual_ops.get(&value_id) {
                Some((actual_kind, _)) if actual_kind == &expected_kind => {}
                Some((actual_kind, _)) => bail!(
                    "lowered debug map kind {actual_kind} does not match operation kind {expected_kind} for {value_id}"
                ),
                None => bail!("lowered debug map missing value id {value_id}"),
            }
        }

        let expected_expr_to_ops = actual_by_expr
            .into_iter()
            .map(|(expr_hash, lowered_op_ids)| LoweredExprOpMap {
                expr_hash,
                lowered_op_ids,
            })
            .collect::<Vec<_>>();
        if ir.debug_map.expr_to_ops != expected_expr_to_ops {
            bail!("lowered debug map expr_to_ops index mismatch");
        }
        Ok(())
    }

    fn verify_lowered_operations(
        &self,
        root: &ProgramRootPayload,
        ir: &LoweredFunctionIr,
        param_types: &[String],
        return_type: &str,
    ) -> Result<()> {
        let mut values = BTreeMap::new();
        let (last, body_ops) = ir
            .operations
            .split_last()
            .ok_or_else(|| anyhow!("lowered IR has no operations"))?;
        self.verify_value_ops(root, body_ops, param_types, &mut values)?;
        match last {
            LoweredOp::Return { value, type_hash } => {
                if type_hash != return_type {
                    bail!("lowered return type does not match function return type");
                }
                let actual = values
                    .get(value)
                    .ok_or_else(|| anyhow!("lowered return references unknown value {value}"))?;
                if actual != type_hash {
                    bail!("lowered return value type mismatch");
                }
                Ok(())
            }
            _ => bail!("lowered IR must end with an explicit return operation"),
        }
    }

    fn verify_value_ops(
        &self,
        root: &ProgramRootPayload,
        operations: &[LoweredOp],
        param_types: &[String],
        values: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        for op in operations {
            match op {
                LoweredOp::Param {
                    id,
                    slot,
                    type_hash,
                } => {
                    let expected = param_types
                        .get(*slot)
                        .ok_or_else(|| anyhow!("lowered param slot out of bounds {slot}"))?;
                    if expected != type_hash {
                        bail!("lowered param slot {slot} type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::ConstI64 {
                    id,
                    value,
                    type_hash,
                } => {
                    value.parse::<i64>()?;
                    if type_hash != &type_hash_for("I64") {
                        bail!("lowered const_i64 type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::ConstBool {
                    id,
                    value: _,
                    type_hash,
                } => {
                    if type_hash != &type_hash_for("Bool") {
                        bail!("lowered const_bool type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::ConstUnit { id, type_hash } => {
                    if type_hash != &type_hash_for("Unit") {
                        bail!("lowered const_unit type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Unary {
                    id,
                    kind,
                    value,
                    type_hash,
                } => {
                    let value_type = value_type(values, value)?;
                    verify_unary_kind(kind, value_type, type_hash)?;
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Binary {
                    id,
                    kind,
                    left,
                    right,
                    type_hash,
                    trap,
                } => {
                    let left_type = value_type(values, left)?;
                    let right_type = value_type(values, right)?;
                    verify_binary_kind(kind, left_type, right_type, type_hash, trap.as_ref())?;
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Call {
                    id,
                    target_symbol_hash,
                    target_abi_symbol,
                    args,
                    type_hash,
                } => {
                    if !is_hash(target_symbol_hash) {
                        bail!("lowered call target is not a symbol hash");
                    }
                    let target = self.root_symbol(root, target_symbol_hash).ok_or_else(|| {
                        anyhow!("lowered call target missing {target_symbol_hash}")
                    })?;
                    let (expected_args, expected_return) =
                        self.signature_parts(&target.signature)?;
                    if args.len() != expected_args.len() {
                        bail!("lowered call arity mismatch for {target_symbol_hash}");
                    }
                    for (idx, arg) in args.iter().enumerate() {
                        let actual = value_type(values, arg)?;
                        if actual != &expected_args[idx] {
                            bail!("lowered call argument {idx} type mismatch");
                        }
                    }
                    if type_hash != &expected_return {
                        bail!("lowered call return type mismatch");
                    }
                    let expected_abi_symbol = if self.definition_is_external(&target.definition)? {
                        Some(
                            self.external_function_metadata(&target.definition)?
                                .link_name,
                        )
                    } else {
                        None
                    };
                    if target_abi_symbol != &expected_abi_symbol {
                        bail!("lowered call ABI symbol does not match target definition");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::If {
                    id,
                    cond,
                    then_block,
                    else_block,
                    type_hash,
                } => {
                    if value_type(values, cond)? != &type_hash_for("Bool") {
                        bail!("lowered if condition must be bool");
                    }
                    let then_type =
                        self.verify_lowered_block(root, then_block, param_types, values)?;
                    let else_type =
                        self.verify_lowered_block(root, else_block, param_types, values)?;
                    if then_type != else_type || then_type != *type_hash {
                        bail!("lowered if branch type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Return { .. } => {
                    bail!("lowered return is only valid as the final function operation");
                }
            }
        }
        Ok(())
    }

    fn verify_lowered_block(
        &self,
        root: &ProgramRootPayload,
        block: &LoweredBlock,
        param_types: &[String],
        parent_values: &BTreeMap<String, String>,
    ) -> Result<String> {
        let mut values = parent_values.clone();
        self.verify_value_ops(root, &block.operations, param_types, &mut values)?;
        value_type(&values, &block.result).cloned()
    }
}

pub(crate) fn lowered_ir_from_artifact_metadata(
    artifact_json: &JsonValue,
) -> Result<LoweredFunctionIr> {
    if artifact_json.get("schema").and_then(JsonValue::as_str) == Some(LOWERED_IR_SCHEMA) {
        return Ok(serde_json::from_value(artifact_json.clone())?);
    }
    let metadata = artifact_json
        .get("metadata")
        .ok_or_else(|| anyhow!("lowered IR artifact missing metadata"))?;
    Ok(serde_json::from_value(metadata.clone())?)
}

fn hash_lowered_ir_json(value: &JsonValue) -> String {
    hash_bytes(BYTES_DOMAIN, canonical_json(value).as_bytes())
}

fn expr_type(payload: &JsonValue, expr_hash: &str) -> Result<String> {
    payload
        .get("type")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("expression missing type {expr_hash}"))
}

fn lower_binary_kind(
    source_op: &str,
    left_type: &str,
    right_type: &str,
    result_type: &str,
) -> Result<String> {
    let i64_hash = type_hash_for("I64");
    let bool_hash = type_hash_for("Bool");
    let kind = match source_op {
        "+" if left_type == i64_hash && right_type == i64_hash && result_type == i64_hash => {
            "add_i64"
        }
        "-" if left_type == i64_hash && right_type == i64_hash && result_type == i64_hash => {
            "sub_i64"
        }
        "*" if left_type == i64_hash && right_type == i64_hash && result_type == i64_hash => {
            "mul_i64"
        }
        "/" if left_type == i64_hash && right_type == i64_hash && result_type == i64_hash => {
            "div_i64"
        }
        "==" if left_type == i64_hash && right_type == i64_hash && result_type == bool_hash => {
            "eq_i64"
        }
        "!=" if left_type == i64_hash && right_type == i64_hash && result_type == bool_hash => {
            "ne_i64"
        }
        "<" if left_type == i64_hash && right_type == i64_hash && result_type == bool_hash => {
            "lt_i64"
        }
        "<=" if left_type == i64_hash && right_type == i64_hash && result_type == bool_hash => {
            "le_i64"
        }
        ">" if left_type == i64_hash && right_type == i64_hash && result_type == bool_hash => {
            "gt_i64"
        }
        ">=" if left_type == i64_hash && right_type == i64_hash && result_type == bool_hash => {
            "ge_i64"
        }
        "&&" if left_type == bool_hash && right_type == bool_hash && result_type == bool_hash => {
            "and_bool"
        }
        "||" if left_type == bool_hash && right_type == bool_hash && result_type == bool_hash => {
            "or_bool"
        }
        _ => bail!("cannot lower binary operator {source_op} with operand/result types"),
    };
    Ok(kind.to_string())
}

fn trap_for_binary(kind: &str) -> Option<LoweredTrap> {
    if kind == "div_i64" {
        Some(LoweredTrap {
            condition: "right_operand_zero".to_string(),
            code: "division_by_zero".to_string(),
        })
    } else {
        None
    }
}

fn verify_binary_kind(
    kind: &str,
    left_type: &str,
    right_type: &str,
    result_type: &str,
    trap: Option<&LoweredTrap>,
) -> Result<()> {
    let source_op = match kind {
        "add_i64" => "+",
        "sub_i64" => "-",
        "mul_i64" => "*",
        "div_i64" => "/",
        "eq_i64" => "==",
        "ne_i64" => "!=",
        "lt_i64" => "<",
        "le_i64" => "<=",
        "gt_i64" => ">",
        "ge_i64" => ">=",
        "and_bool" => "&&",
        "or_bool" => "||",
        _ => bail!("unknown lowered binary kind {kind}"),
    };
    let expected = lower_binary_kind(source_op, left_type, right_type, result_type)?;
    if expected != kind {
        bail!("lowered binary kind/type mismatch");
    }
    match (kind, trap) {
        ("div_i64", Some(LoweredTrap { condition, code }))
            if condition == "right_operand_zero" && code == "division_by_zero" =>
        {
            Ok(())
        }
        ("div_i64", _) => bail!("lowered div_i64 must include a division_by_zero trap"),
        (_, None) => Ok(()),
        (_, Some(_)) => bail!("unexpected trap on lowered binary kind {kind}"),
    }
}

fn lower_unary_kind(source_op: &str, input_type: &str, result_type: &str) -> Result<String> {
    let i64_hash = type_hash_for("I64");
    let bool_hash = type_hash_for("Bool");
    let kind = match source_op {
        "-" if input_type == i64_hash && result_type == i64_hash => "neg_i64",
        "!" if input_type == bool_hash && result_type == bool_hash => "not_bool",
        _ => bail!("cannot lower unary operator {source_op} with operand/result types"),
    };
    Ok(kind.to_string())
}

fn verify_unary_kind(kind: &str, input_type: &str, result_type: &str) -> Result<()> {
    let source_op = match kind {
        "neg_i64" => "-",
        "not_bool" => "!",
        _ => bail!("unknown lowered unary kind {kind}"),
    };
    let expected = lower_unary_kind(source_op, input_type, result_type)?;
    if expected != kind {
        bail!("lowered unary kind/type mismatch");
    }
    Ok(())
}

fn insert_value(values: &mut BTreeMap<String, String>, id: &str, type_hash: &str) -> Result<()> {
    if values
        .insert(id.to_string(), type_hash.to_string())
        .is_some()
    {
        bail!("duplicate lowered value id {id}");
    }
    Ok(())
}

fn value_type<'a>(values: &'a BTreeMap<String, String>, id: &str) -> Result<&'a String> {
    values
        .get(id)
        .ok_or_else(|| anyhow!("unknown lowered value id {id}"))
}

pub(crate) fn lowered_op_id_for_value(value_id: &str) -> String {
    format!("op:{value_id}")
}

pub(crate) fn lowered_op_value_id(op: &LoweredOp) -> Option<&str> {
    match op {
        LoweredOp::Param { id, .. }
        | LoweredOp::ConstI64 { id, .. }
        | LoweredOp::ConstBool { id, .. }
        | LoweredOp::ConstUnit { id, .. }
        | LoweredOp::Unary { id, .. }
        | LoweredOp::Binary { id, .. }
        | LoweredOp::Call { id, .. }
        | LoweredOp::If { id, .. } => Some(id),
        LoweredOp::Return { .. } => None,
    }
}

pub(crate) fn lowered_op_kind_name(op: &LoweredOp) -> &'static str {
    match op {
        LoweredOp::Param { .. } => "param",
        LoweredOp::ConstI64 { .. } => "const_i64",
        LoweredOp::ConstBool { .. } => "const_bool",
        LoweredOp::ConstUnit { .. } => "const_unit",
        LoweredOp::Unary { .. } => "unary",
        LoweredOp::Binary { .. } => "binary",
        LoweredOp::Call { .. } => "call",
        LoweredOp::If { .. } => "if",
        LoweredOp::Return { .. } => "return",
    }
}

pub(crate) fn lowered_value_debug_ops(
    ir: &LoweredFunctionIr,
) -> Result<BTreeMap<String, LoweredDebugOp>> {
    let expected_values = lowered_value_debug_infos(&ir.operations)?
        .into_keys()
        .collect::<BTreeSet<_>>();
    let mut out = BTreeMap::new();
    for op in &ir.debug_map.operations {
        if expected_values.contains(&op.value_id) {
            out.insert(op.value_id.clone(), op.clone());
        }
    }
    Ok(out)
}

fn lowered_value_debug_infos(operations: &[LoweredOp]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    collect_lowered_value_debug_infos(operations, &mut out)?;
    Ok(out)
}

fn collect_lowered_value_debug_infos(
    operations: &[LoweredOp],
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    for op in operations {
        if let Some(value_id) = lowered_op_value_id(op)
            && out
                .insert(value_id.to_string(), lowered_op_kind_name(op).to_string())
                .is_some()
        {
            bail!("duplicate lowered value id {value_id}");
        }
        if let LoweredOp::If {
            then_block,
            else_block,
            ..
        } = op
        {
            collect_lowered_value_debug_infos(&then_block.operations, out)?;
            collect_lowered_value_debug_infos(&else_block.operations, out)?;
        }
    }
    Ok(())
}

fn is_hash(value: &str) -> bool {
    value.starts_with("sha256:")
}

fn local_lowered_at_depth(
    locals: &[LocalLoweredBinding],
    depth: usize,
) -> Option<&LocalLoweredBinding> {
    locals
        .len()
        .checked_sub(depth + 1)
        .and_then(|idx| locals.get(idx))
}
