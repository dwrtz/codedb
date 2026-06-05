use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::artifact::CacheKeyInput;
use crate::backend::ArtifactKind;
use crate::model::{ProgramRootPayload, RootSymbolPayload, validate_projection_identifier};
use crate::store::{CodeDb, canonical_json, hash_bytes};
use crate::types::{TypeDefinition, TypeSpec, type_hash_for};
use crate::{BYTES_DOMAIN, DEFAULT_NATIVE_TARGET, MAIN_BRANCH};

pub(crate) const LOWERED_IR_SCHEMA: &str = "codedb/lowered-function-ir/v2";
pub(crate) const LOWERED_DEBUG_MAP_SCHEMA: &str = "codedb/lowered-debug-map/v1";
const LOWERED_IR_INSPECTION_SCHEMA: &str = "codedb/lowered-ir-inspection/v1";
const LOWERING_BACKEND_ID: &str = "lowering-v1";
const LOWERING_TARGET: &str = "target-independent-memory-ir-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LoweredFunctionIr {
    pub(crate) schema: String,
    pub(crate) symbol_hash: String,
    pub(crate) function_def_hash: String,
    pub(crate) function_sig_hash: String,
    pub(crate) typed_body_expr_hash: String,
    pub(crate) params: Vec<LoweredParamSlot>,
    #[serde(default)]
    pub(crate) locals: Vec<LoweredLocalSlot>,
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
pub(crate) struct LoweredLocalSlot {
    pub(crate) slot: usize,
    pub(crate) type_hash: String,
    #[serde(
        default = "default_slot_size_bytes",
        skip_serializing_if = "is_default_slot_size"
    )]
    pub(crate) size_bytes: u64,
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LoweredPlace {
    Param {
        slot: usize,
        type_hash: String,
        #[serde(default, skip_serializing_if = "is_false")]
        indirect: bool,
    },
    Local {
        slot: usize,
        type_hash: String,
    },
    Field {
        base: String,
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field_symbol: Option<String>,
        owner_type_hash: String,
        #[serde(default, skip_serializing_if = "is_zero_u64")]
        offset_bytes: u64,
        type_hash: String,
    },
    Index {
        base: String,
        index: String,
        element_type_hash: String,
        type_hash: String,
    },
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
    BorrowShared {
        id: String,
        address: String,
        region: String,
        referent_type_hash: String,
        type_hash: String,
    },
    BorrowMut {
        id: String,
        address: String,
        region: String,
        referent_type_hash: String,
        type_hash: String,
    },
    DerefShared {
        id: String,
        reference: String,
        referent_type_hash: String,
    },
    DerefMut {
        id: String,
        reference: String,
        referent_type_hash: String,
    },
    AddrOfParam {
        id: String,
        place: LoweredPlace,
    },
    AddrOfLocal {
        id: String,
        place: LoweredPlace,
    },
    AddrOfField {
        id: String,
        place: LoweredPlace,
    },
    AddrOfIndex {
        id: String,
        place: LoweredPlace,
    },
    Load {
        id: String,
        address: String,
        type_hash: String,
    },
    Store {
        address: String,
        value: String,
        type_hash: String,
    },
    Copy {
        id: String,
        value: String,
        type_hash: String,
    },
    Move {
        id: String,
        address: String,
        type_hash: String,
    },
    Drop {
        address: String,
        type_hash: String,
    },
    BorrowDebug {
        address: String,
        mutable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
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

struct LoweredAddress {
    operations: Vec<LoweredOp>,
    address: String,
    type_hash: String,
}

struct LoweredFieldInfo {
    type_hash: String,
    field_symbol: Option<String>,
    offset_bytes: u64,
}

#[derive(Debug, Clone)]
struct LocalLoweredBinding {
    slot: usize,
    type_hash: String,
}

#[derive(Default)]
struct LowerCtx {
    next_value: usize,
    next_local: usize,
    local_slots: Vec<LoweredLocalSlot>,
    debug_operations: Vec<LoweredDebugOp>,
}

impl LowerCtx {
    fn value(&mut self) -> String {
        let value = format!("v{}", self.next_value);
        self.next_value += 1;
        value
    }

    fn local_slot(&mut self, type_hash: String, size_bytes: u64) -> usize {
        let slot = self.next_local;
        self.next_local += 1;
        self.local_slots.push(LoweredLocalSlot {
            slot,
            type_hash,
            size_bytes,
        });
        slot
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
        let allowed_regions = self
            .signature_region_params(&entry.signature)?
            .into_iter()
            .map(|param| param.region)
            .collect::<BTreeSet<_>>();
        for type_hash in &param_types {
            self.ensure_addressable_ir_type(root, type_hash)?;
        }
        self.ensure_lowerable_return_type(&return_type)?;
        let body = self.function_body_hash(&entry.definition)?;
        let actual_return = self.verify_expr_type(&body, root, &param_types, &allowed_regions)?;
        if actual_return != return_type {
            bail!(
                "function body type {} does not match return type {}",
                actual_return,
                return_type
            );
        }

        let mut ctx = LowerCtx::default();
        let mut lowered = self.lower_expr(root, &body, &param_types, &mut ctx, &mut Vec::new())?;
        let local_slots = ctx.local_slots.clone();
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
            locals: local_slots,
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
                self.lower_place_value(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "local_ref" => {
                self.lower_place_value(root, expr_hash, &type_hash, param_types, ctx, locals)
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
                let slot_size = stack_slot_size_bytes(self.layout_size_bytes(root, &binding_type)?);
                let slot = ctx.local_slot(binding_type.clone(), slot_size);
                let address = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_local", &address);
                let mut operations = vec![LoweredOp::AddrOfLocal {
                    id: address.clone(),
                    place: LoweredPlace::Local {
                        slot,
                        type_hash: binding_type.clone(),
                    },
                }];
                if self
                    .get_payload(value_hash)?
                    .get("expr_kind")
                    .and_then(JsonValue::as_str)
                    == Some("record_literal")
                {
                    operations.extend(self.lower_record_init_to_address(
                        root,
                        value_hash,
                        &binding_type,
                        &address,
                        param_types,
                        ctx,
                        locals,
                    )?);
                } else if self.is_aggregate_ir_type(root, &binding_type)?
                    && self.expr_is_place(value_hash)?
                {
                    operations.extend(self.lower_aggregate_place_init_to_address(
                        root,
                        value_hash,
                        &binding_type,
                        &address,
                        param_types,
                        ctx,
                        locals,
                    )?);
                } else {
                    let value = self.lower_expr(root, value_hash, param_types, ctx, locals)?;
                    if !self.type_assignable_in_root(root, &value.type_hash, &binding_type)? {
                        bail!("let binding type mismatch while lowering");
                    }
                    operations.extend(value.operations);
                    operations.push(LoweredOp::Store {
                        address: address.clone(),
                        value: value.value,
                        type_hash: binding_type.clone(),
                    });
                }
                locals.push(LocalLoweredBinding {
                    slot,
                    type_hash: binding_type.clone(),
                });
                let body = self.lower_expr(root, body_hash, param_types, ctx, locals);
                locals.pop();
                let body = body?;
                operations.extend(body.operations);
                if self.type_requires_drop_scaffold(root, &binding_type)? {
                    operations.push(LoweredOp::Drop {
                        address,
                        type_hash: binding_type,
                    });
                }
                Ok(LoweredExpr {
                    operations,
                    value: body.value,
                    type_hash: body.type_hash,
                })
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
                let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
                if target.type_hash != referent_type_hash {
                    bail!("borrow_shared referent type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "borrow_shared", &id);
                let mut operations = target.operations;
                operations.push(LoweredOp::BorrowDebug {
                    address: target.address.clone(),
                    mutable: false,
                    region: Some(region.clone()),
                    type_hash: referent_type_hash.clone(),
                });
                operations.push(LoweredOp::BorrowShared {
                    id: id.clone(),
                    address: target.address,
                    region,
                    referent_type_hash,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
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
                let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
                if target.type_hash != referent_type_hash {
                    bail!("borrow_mut referent type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "borrow_mut", &id);
                let mut operations = target.operations;
                operations.push(LoweredOp::BorrowDebug {
                    address: target.address.clone(),
                    mutable: true,
                    region: Some(region.clone()),
                    type_hash: referent_type_hash.clone(),
                });
                operations.push(LoweredOp::BorrowMut {
                    id: id.clone(),
                    address: target.address,
                    region,
                    referent_type_hash,
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
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
                let target = self.lower_place(root, target_hash, param_types, ctx, locals)?;
                let value = self.lower_expr(root, value_hash, param_types, ctx, locals)?;
                if !self.type_assignable_in_root(root, &value.type_hash, &target.type_hash)? {
                    bail!("assignment type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "const_unit", &id);
                let mut operations = target.operations;
                operations.extend(value.operations);
                operations.push(LoweredOp::Store {
                    address: target.address,
                    value: value.value,
                    type_hash: target.type_hash,
                });
                operations.push(LoweredOp::ConstUnit {
                    id: id.clone(),
                    type_hash: type_hash.clone(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash,
                })
            }
            "record_literal" => {
                bail!("lowering v1 supports record literals only as typed let initializers")
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
            "field_access" => {
                self.lower_place_value(root, expr_hash, &type_hash, param_types, ctx, locals)
            }
            "enum_construct" | "case" => {
                bail!("lowering v1 does not support aggregate expression kind {expr_kind}")
            }
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn lower_place_value(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        let lowered = self.lower_place(root, expr_hash, param_types, ctx, locals)?;
        if self.type_is_move_only(root, type_hash)? {
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "move", &id);
            let mut operations = lowered.operations;
            operations.push(LoweredOp::Move {
                id: id.clone(),
                address: lowered.address,
                type_hash: type_hash.to_string(),
            });
            return Ok(LoweredExpr {
                operations,
                value: id,
                type_hash: type_hash.to_string(),
            });
        }
        if self.is_aggregate_ir_type(root, type_hash)? {
            let id = ctx.value();
            ctx.push_debug_op(expr_hash, "copy", &id);
            let mut operations = lowered.operations;
            operations.push(LoweredOp::Copy {
                id: id.clone(),
                value: lowered.address,
                type_hash: type_hash.to_string(),
            });
            return Ok(LoweredExpr {
                operations,
                value: id,
                type_hash: type_hash.to_string(),
            });
        }
        let id = ctx.value();
        ctx.push_debug_op(expr_hash, "load", &id);
        let mut operations = lowered.operations;
        operations.push(LoweredOp::Load {
            id: id.clone(),
            address: lowered.address,
            type_hash: type_hash.to_string(),
        });
        Ok(LoweredExpr {
            operations,
            value: id,
            type_hash: type_hash.to_string(),
        })
    }

    fn lower_reference_value_for_place(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        type_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredExpr> {
        match self.type_spec(type_hash)? {
            TypeSpec::Reference { .. } => {}
            _ => bail!("place reference lowering requires reference type"),
        }
        let payload = self.get_payload(expr_hash)?;
        match payload.get("expr_kind").and_then(JsonValue::as_str) {
            Some("param_ref" | "local_ref" | "field_access") => {
                let lowered = self.lower_place(root, expr_hash, param_types, ctx, locals)?;
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "load", &id);
                let mut operations = lowered.operations;
                operations.push(LoweredOp::Load {
                    id: id.clone(),
                    address: lowered.address,
                    type_hash: type_hash.to_string(),
                });
                Ok(LoweredExpr {
                    operations,
                    value: id,
                    type_hash: type_hash.to_string(),
                })
            }
            _ => self.lower_expr(root, expr_hash, param_types, ctx, locals),
        }
    }

    fn lower_place(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<LoweredAddress> {
        let payload = self.get_payload(expr_hash)?;
        let type_hash = expr_type(&payload, expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        match expr_kind {
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
                ctx.push_debug_op(expr_hash, "addr_of_param", &id);
                Ok(LoweredAddress {
                    operations: vec![LoweredOp::AddrOfParam {
                        id: id.clone(),
                        place: LoweredPlace::Param {
                            slot,
                            type_hash: type_hash.clone(),
                            indirect: self.is_aggregate_ir_type(root, &type_hash)?,
                        },
                    }],
                    address: id,
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
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_local", &id);
                Ok(LoweredAddress {
                    operations: vec![LoweredOp::AddrOfLocal {
                        id: id.clone(),
                        place: LoweredPlace::Local {
                            slot: binding.slot,
                            type_hash: type_hash.clone(),
                        },
                    }],
                    address: id,
                    type_hash,
                })
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
                validate_projection_identifier("record field", field)?;
                let target_payload = self.get_payload(target_hash)?;
                let target_type = expr_type(&target_payload, target_hash)?;
                let target = match self.type_spec(&target_type)? {
                    TypeSpec::Reference {
                        mutable, referent, ..
                    } => {
                        let lowered_ref = self.lower_reference_value_for_place(
                            root,
                            target_hash,
                            &target_type,
                            param_types,
                            ctx,
                            locals,
                        )?;
                        let id = ctx.value();
                        let debug_kind = if mutable { "deref_mut" } else { "deref_shared" };
                        ctx.push_debug_op(expr_hash, debug_kind, &id);
                        let mut operations = lowered_ref.operations;
                        if mutable {
                            operations.push(LoweredOp::DerefMut {
                                id: id.clone(),
                                reference: lowered_ref.value,
                                referent_type_hash: referent.clone(),
                            });
                        } else {
                            operations.push(LoweredOp::DerefShared {
                                id: id.clone(),
                                reference: lowered_ref.value,
                                referent_type_hash: referent.clone(),
                            });
                        }
                        LoweredAddress {
                            operations,
                            address: id,
                            type_hash: referent,
                        }
                    }
                    _ => self.lower_place(root, target_hash, param_types, ctx, locals)?,
                };
                let field_info = self.lowered_record_field(root, &target.type_hash, field)?;
                if field_info.type_hash != type_hash {
                    bail!("field_access type mismatch while lowering");
                }
                let id = ctx.value();
                ctx.push_debug_op(expr_hash, "addr_of_field", &id);
                let mut operations = target.operations;
                operations.push(LoweredOp::AddrOfField {
                    id: id.clone(),
                    place: LoweredPlace::Field {
                        base: target.address,
                        field: field.to_string(),
                        field_symbol: field_info.field_symbol,
                        owner_type_hash: target.type_hash,
                        offset_bytes: field_info.offset_bytes,
                        type_hash: type_hash.clone(),
                    },
                });
                Ok(LoweredAddress {
                    operations,
                    address: id,
                    type_hash,
                })
            }
            other => bail!("expression kind {other} is not an addressable place"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_record_init_to_address(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        target_type: &str,
        target_address: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<Vec<LoweredOp>> {
        let payload = self.get_payload(expr_hash)?;
        if payload.get("expr_kind").and_then(JsonValue::as_str) != Some("record_literal") {
            bail!("record initializer must be record_literal");
        }
        let mut operations = Vec::new();
        for field in payload
            .get("fields")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("record_literal missing fields"))?
        {
            let name = field
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("record field missing name"))?;
            let value_hash = field
                .get("value")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("record field missing value"))?;
            let field_info = self.lowered_record_field(root, target_type, name)?;
            let value = self.lower_expr(root, value_hash, param_types, ctx, locals)?;
            if !self.type_assignable_in_root(root, &value.type_hash, &field_info.type_hash)? {
                bail!("record initializer field {name} type mismatch while lowering");
            }
            let store_type_hash = if value.type_hash == field_info.type_hash {
                field_info.type_hash.clone()
            } else {
                value.type_hash.clone()
            };
            operations.extend(value.operations);
            let field_address = ctx.value();
            ctx.push_debug_op(expr_hash, "addr_of_field", &field_address);
            operations.push(LoweredOp::AddrOfField {
                id: field_address.clone(),
                place: LoweredPlace::Field {
                    base: target_address.to_string(),
                    field: name.to_string(),
                    field_symbol: field_info.field_symbol,
                    owner_type_hash: target_type.to_string(),
                    offset_bytes: field_info.offset_bytes,
                    type_hash: store_type_hash.clone(),
                },
            });
            operations.push(LoweredOp::Store {
                address: field_address,
                value: value.value,
                type_hash: store_type_hash,
            });
        }
        Ok(operations)
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_aggregate_place_init_to_address(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        target_type: &str,
        target_address: &str,
        param_types: &[String],
        ctx: &mut LowerCtx,
        locals: &mut Vec<LocalLoweredBinding>,
    ) -> Result<Vec<LoweredOp>> {
        let source_payload = self.get_payload(expr_hash)?;
        let source_type = expr_type(&source_payload, expr_hash)?;
        if !self.type_assignable_in_root(root, &source_type, target_type)? {
            bail!("aggregate initializer type mismatch while lowering");
        }
        let source = self.lower_place(root, expr_hash, param_types, ctx, locals)?;
        let mut operations = source.operations;
        let scaffold_id = ctx.value();
        if self.type_is_move_only(root, &source_type)? {
            ctx.push_debug_op(expr_hash, "move", &scaffold_id);
            operations.push(LoweredOp::Move {
                id: scaffold_id,
                address: source.address.clone(),
                type_hash: source_type.clone(),
            });
        } else {
            ctx.push_debug_op(expr_hash, "copy", &scaffold_id);
            operations.push(LoweredOp::Copy {
                id: scaffold_id,
                value: source.address.clone(),
                type_hash: source_type.clone(),
            });
        }

        for field in self.aggregate_record_fields(root, target_type)? {
            let source_field_info =
                self.lowered_record_field(root, &source.type_hash, &field.name)?;
            if !self.type_assignable_in_root(
                root,
                &source_field_info.type_hash,
                &field.type_hash,
            )? {
                bail!(
                    "aggregate initializer field {} type mismatch while lowering",
                    field.name
                );
            }

            let source_field_address = ctx.value();
            ctx.push_debug_op(expr_hash, "addr_of_field", &source_field_address);
            operations.push(LoweredOp::AddrOfField {
                id: source_field_address.clone(),
                place: LoweredPlace::Field {
                    base: source.address.clone(),
                    field: field.name.clone(),
                    field_symbol: source_field_info.field_symbol,
                    owner_type_hash: source.type_hash.clone(),
                    offset_bytes: source_field_info.offset_bytes,
                    type_hash: source_field_info.type_hash.clone(),
                },
            });

            let field_value = ctx.value();
            ctx.push_debug_op(expr_hash, "load", &field_value);
            operations.push(LoweredOp::Load {
                id: field_value.clone(),
                address: source_field_address,
                type_hash: source_field_info.type_hash.clone(),
            });

            let target_field_info = self.lowered_record_field(root, target_type, &field.name)?;
            let store_type_hash = if source_field_info.type_hash == target_field_info.type_hash {
                target_field_info.type_hash.clone()
            } else {
                source_field_info.type_hash.clone()
            };
            let target_field_address = ctx.value();
            ctx.push_debug_op(expr_hash, "addr_of_field", &target_field_address);
            operations.push(LoweredOp::AddrOfField {
                id: target_field_address.clone(),
                place: LoweredPlace::Field {
                    base: target_address.to_string(),
                    field: field.name,
                    field_symbol: target_field_info.field_symbol,
                    owner_type_hash: target_type.to_string(),
                    offset_bytes: target_field_info.offset_bytes,
                    type_hash: store_type_hash.clone(),
                },
            });
            operations.push(LoweredOp::Store {
                address: target_field_address,
                value: field_value,
                type_hash: store_type_hash,
            });
        }
        Ok(operations)
    }

    fn lowered_record_field(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        field: &str,
    ) -> Result<LoweredFieldInfo> {
        let offset_bytes = self.layout_field_offset_bytes(root, type_hash, field)?;
        if let TypeSpec::Named { type_symbol, .. } = self.type_spec(type_hash)? {
            let entry = self
                .root_type(root, &type_symbol)
                .ok_or_else(|| anyhow!("named record missing from root {type_symbol}"))?;
            let TypeDefinition::Record { fields, .. } = self.type_definition(&entry.type_def)?
            else {
                bail!("field access requires record type");
            };
            return fields
                .into_iter()
                .find(|candidate| candidate.name == field)
                .map(|candidate| LoweredFieldInfo {
                    type_hash: candidate.type_hash,
                    field_symbol: Some(candidate.member_symbol),
                    offset_bytes,
                })
                .ok_or_else(|| anyhow!("record has no field {field}"));
        }

        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Record(fields) => fields
                .into_iter()
                .find(|candidate| candidate.name == field)
                .map(|candidate| LoweredFieldInfo {
                    type_hash: candidate.type_hash,
                    field_symbol: None,
                    offset_bytes,
                })
                .ok_or_else(|| anyhow!("record has no field {field}")),
            other => bail!(
                "field access requires record type, got {}",
                other.to_source(self)?
            ),
        }
    }

    fn aggregate_record_fields(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<Vec<crate::types::TypeFieldSpec>> {
        match self.type_spec_in_root(root, type_hash)? {
            TypeSpec::Record(fields) => Ok(fields),
            other => bail!(
                "aggregate place initializer requires record type, got {}",
                other.to_source(self)?
            ),
        }
    }

    fn expr_is_place(&self, expr_hash: &str) -> Result<bool> {
        let payload = self.get_payload(expr_hash)?;
        Ok(matches!(
            payload.get("expr_kind").and_then(JsonValue::as_str),
            Some("param_ref" | "local_ref" | "field_access")
        ))
    }

    fn layout_size_bytes(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<u64> {
        self.compute_type_layout(root, type_hash, DEFAULT_NATIVE_TARGET)?
            .metadata
            .get("size_bytes")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("type layout missing size_bytes for {type_hash}"))
    }

    fn type_is_move_only(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<bool> {
        let layout = self.compute_type_layout(root, type_hash, DEFAULT_NATIVE_TARGET)?;
        match layout.metadata.get("copy_kind").and_then(JsonValue::as_str) {
            Some("copy") => Ok(false),
            Some("move_only") => Ok(true),
            Some(other) => bail!("unknown copy_kind {other} for type {type_hash}"),
            None => bail!("type layout missing copy_kind for {type_hash}"),
        }
    }

    fn type_requires_drop_scaffold(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
    ) -> Result<bool> {
        let layout = self.compute_type_layout(root, type_hash, DEFAULT_NATIVE_TARGET)?;
        let move_only = match layout.metadata.get("copy_kind").and_then(JsonValue::as_str) {
            Some("copy") => false,
            Some("move_only") => true,
            Some(other) => bail!("unknown copy_kind {other} for type {type_hash}"),
            None => bail!("type layout missing copy_kind for {type_hash}"),
        };
        let needs_drop = match layout.metadata.get("drop_kind").and_then(JsonValue::as_str) {
            Some("trivial") => false,
            Some("needs_drop") => true,
            Some(other) => bail!("unknown drop_kind {other} for type {type_hash}"),
            None => bail!("type layout missing drop_kind for {type_hash}"),
        };
        let contains_reference = layout
            .metadata
            .get("contains_reference")
            .and_then(JsonValue::as_bool)
            .ok_or_else(|| anyhow!("type layout missing contains_reference for {type_hash}"))?;
        Ok(move_only || needs_drop || contains_reference)
    }

    fn layout_field_offset_bytes(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        field: &str,
    ) -> Result<u64> {
        let layout = self.compute_type_layout(root, type_hash, DEFAULT_NATIVE_TARGET)?;
        layout
            .metadata
            .get("fields")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .find(|entry| entry.get("name").and_then(JsonValue::as_str) == Some(field))
            .and_then(|entry| entry.get("offset_bytes").and_then(JsonValue::as_u64))
            .ok_or_else(|| anyhow!("type layout missing offset for field {field}"))
    }

    fn ensure_lowerable_return_type(&self, type_hash: &str) -> Result<()> {
        let type_name = self.type_name(type_hash)?;
        match type_name.as_str() {
            "i64" | "bool" | "unit" => Ok(()),
            _ => bail!("lowering v1 does not support aggregate return type {type_name}"),
        }
    }

    fn ensure_addressable_ir_type(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<()> {
        self.type_spec_in_root(root, type_hash)?;
        Ok(())
    }

    fn is_aggregate_ir_type(&self, root: &ProgramRootPayload, type_hash: &str) -> Result<bool> {
        Ok(matches!(
            self.type_spec_in_root(root, type_hash)?,
            TypeSpec::Record(_) | TypeSpec::Enum(_) | TypeSpec::FixedArray { .. }
        ))
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
            if let Err(err) = self.verify_lowered_ir(&root, ir) {
                last_error = Some(err);
                continue;
            }
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
        let allowed_regions = self
            .signature_region_params(&ir.function_sig_hash)?
            .into_iter()
            .map(|param| param.region)
            .collect::<BTreeSet<_>>();
        let actual_return = self.verify_expr_type(
            &ir.typed_body_expr_hash,
            root,
            &param_types,
            &allowed_regions,
        )?;
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
        for (slot, local) in ir.locals.iter().enumerate() {
            if local.slot != slot || !is_hash(&local.type_hash) {
                bail!("lowered IR local slot {slot} mismatch");
            }
            self.type_spec(&local.type_hash)?;
            if local.size_bytes == 0 {
                bail!("lowered IR local slot {slot} has zero size");
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
        let mut addresses = BTreeMap::new();
        let (last, body_ops) = ir
            .operations
            .split_last()
            .ok_or_else(|| anyhow!("lowered IR has no operations"))?;
        self.verify_value_ops(
            root,
            body_ops,
            param_types,
            &ir.locals,
            &mut values,
            &mut addresses,
        )?;
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
        local_slots: &[LoweredLocalSlot],
        values: &mut BTreeMap<String, String>,
        addresses: &mut BTreeMap<String, String>,
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
                        if !self.type_assignable_in_root(root, actual, &expected_args[idx])? {
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
                    let then_type = self.verify_lowered_block(
                        root,
                        then_block,
                        param_types,
                        local_slots,
                        values,
                        addresses,
                    )?;
                    let else_type = self.verify_lowered_block(
                        root,
                        else_block,
                        param_types,
                        local_slots,
                        values,
                        addresses,
                    )?;
                    if then_type != else_type || then_type != *type_hash {
                        bail!("lowered if branch type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::BorrowShared {
                    id,
                    address,
                    region,
                    referent_type_hash,
                    type_hash,
                } => {
                    if !is_hash(region) {
                        bail!("lowered borrow_shared region is not a hash");
                    }
                    if address_type(addresses, address)? != referent_type_hash {
                        bail!("lowered borrow_shared referent type mismatch");
                    }
                    match self.type_spec(type_hash)? {
                        TypeSpec::Reference {
                            region: actual_region,
                            mutable: false,
                            referent,
                        } if actual_region == *region && referent == *referent_type_hash => {}
                        _ => bail!("lowered borrow_shared type mismatch"),
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::BorrowMut {
                    id,
                    address,
                    region,
                    referent_type_hash,
                    type_hash,
                } => {
                    if !is_hash(region) {
                        bail!("lowered borrow_mut region is not a hash");
                    }
                    if address_type(addresses, address)? != referent_type_hash {
                        bail!("lowered borrow_mut referent type mismatch");
                    }
                    match self.type_spec(type_hash)? {
                        TypeSpec::Reference {
                            region: actual_region,
                            mutable: true,
                            referent,
                        } if actual_region == *region && referent == *referent_type_hash => {}
                        _ => bail!("lowered borrow_mut type mismatch"),
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::DerefShared {
                    id,
                    reference,
                    referent_type_hash,
                } => {
                    match self.type_spec(value_type(values, reference)?)? {
                        TypeSpec::Reference {
                            mutable: false,
                            referent,
                            ..
                        } if referent == *referent_type_hash => {}
                        _ => bail!("lowered deref_shared requires shared reference value"),
                    }
                    insert_address(addresses, id, referent_type_hash)?;
                    if self.is_aggregate_ir_type(root, referent_type_hash)? {
                        insert_value(values, id, referent_type_hash)?;
                    }
                }
                LoweredOp::DerefMut {
                    id,
                    reference,
                    referent_type_hash,
                } => {
                    match self.type_spec(value_type(values, reference)?)? {
                        TypeSpec::Reference {
                            mutable: true,
                            referent,
                            ..
                        } if referent == *referent_type_hash => {}
                        _ => bail!("lowered deref_mut requires mutable reference value"),
                    }
                    insert_address(addresses, id, referent_type_hash)?;
                    if self.is_aggregate_ir_type(root, referent_type_hash)? {
                        insert_value(values, id, referent_type_hash)?;
                    }
                }
                LoweredOp::AddrOfParam { id, place } => {
                    let LoweredPlace::Param {
                        slot,
                        type_hash,
                        indirect,
                    } = place
                    else {
                        bail!("addr_of_param must contain a param place");
                    };
                    let expected = param_types.get(*slot).ok_or_else(|| {
                        anyhow!("lowered addr_of_param slot out of bounds {slot}")
                    })?;
                    if expected != type_hash {
                        bail!("lowered addr_of_param slot {slot} type mismatch");
                    }
                    if *indirect != self.is_aggregate_ir_type(root, type_hash)? {
                        bail!("lowered addr_of_param indirect flag mismatch");
                    }
                    insert_address(addresses, id, type_hash)?;
                    if self.is_aggregate_ir_type(root, type_hash)? {
                        insert_value(values, id, type_hash)?;
                    }
                }
                LoweredOp::AddrOfLocal { id, place } => {
                    let LoweredPlace::Local { slot, type_hash } = place else {
                        bail!("addr_of_local must contain a local place");
                    };
                    let expected = local_slots.get(*slot).ok_or_else(|| {
                        anyhow!("lowered addr_of_local slot out of bounds {slot}")
                    })?;
                    if expected.slot != *slot || expected.type_hash != *type_hash {
                        bail!("lowered addr_of_local slot {slot} type mismatch");
                    }
                    insert_address(addresses, id, type_hash)?;
                    if self.is_aggregate_ir_type(root, type_hash)? {
                        insert_value(values, id, type_hash)?;
                    }
                }
                LoweredOp::AddrOfField { id, place } => {
                    let LoweredPlace::Field {
                        base,
                        field,
                        field_symbol,
                        owner_type_hash,
                        offset_bytes,
                        type_hash,
                    } = place
                    else {
                        bail!("addr_of_field must contain a field place");
                    };
                    let base_type = address_type(addresses, base)?;
                    if base_type != owner_type_hash {
                        bail!("lowered addr_of_field owner type mismatch");
                    }
                    let field_info = self.lowered_record_field(root, owner_type_hash, field)?;
                    if field_info.type_hash != *type_hash
                        && !self.type_assignable_in_root(root, type_hash, &field_info.type_hash)?
                    {
                        bail!("lowered addr_of_field type mismatch");
                    }
                    if &field_info.field_symbol != field_symbol {
                        bail!("lowered addr_of_field symbol mismatch");
                    }
                    if field_info.offset_bytes != *offset_bytes {
                        bail!("lowered addr_of_field offset mismatch");
                    }
                    insert_address(addresses, id, type_hash)?;
                    if self.is_aggregate_ir_type(root, type_hash)? {
                        insert_value(values, id, type_hash)?;
                    }
                }
                LoweredOp::AddrOfIndex { id, place } => {
                    let LoweredPlace::Index {
                        base,
                        index,
                        element_type_hash,
                        type_hash,
                    } = place
                    else {
                        bail!("addr_of_index must contain an index place");
                    };
                    let base_type = address_type(addresses, base)?;
                    if value_type(values, index)? != &type_hash_for("I64") {
                        bail!("lowered addr_of_index index must be i64");
                    }
                    match self.type_spec_in_root(root, base_type)? {
                        TypeSpec::FixedArray { element, .. } => {
                            if element != *element_type_hash || element != *type_hash {
                                bail!("lowered addr_of_index element type mismatch");
                            }
                        }
                        other => bail!(
                            "lowered addr_of_index requires array place, got {}",
                            other.to_source(self)?
                        ),
                    }
                    insert_address(addresses, id, type_hash)?;
                }
                LoweredOp::Load {
                    id,
                    address,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered load type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Store {
                    address,
                    value,
                    type_hash,
                } => {
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered store address type mismatch");
                    }
                    if value_type(values, value)? != type_hash {
                        bail!("lowered store value type mismatch");
                    }
                }
                LoweredOp::Copy {
                    id,
                    value,
                    type_hash,
                } => {
                    if self.type_is_move_only(root, type_hash)? {
                        bail!("lowered copy requires a copy type");
                    }
                    if value_type(values, value)? != type_hash {
                        bail!("lowered copy type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Move {
                    id,
                    address,
                    type_hash,
                } => {
                    if !self.type_is_move_only(root, type_hash)? {
                        bail!("lowered move requires a move-only type");
                    }
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered move type mismatch");
                    }
                    insert_value(values, id, type_hash)?;
                }
                LoweredOp::Drop { address, type_hash } => {
                    if !self.type_requires_drop_scaffold(root, type_hash)? {
                        bail!("lowered drop requires a drop-relevant type");
                    }
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered drop type mismatch");
                    }
                }
                LoweredOp::BorrowDebug {
                    address,
                    mutable: _,
                    region,
                    type_hash,
                } => {
                    if let Some(region) = region
                        && !is_hash(region)
                    {
                        bail!("lowered borrow_debug region is not a hash");
                    }
                    if address_type(addresses, address)? != type_hash {
                        bail!("lowered borrow_debug type mismatch");
                    }
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
        local_slots: &[LoweredLocalSlot],
        parent_values: &BTreeMap<String, String>,
        parent_addresses: &BTreeMap<String, String>,
    ) -> Result<String> {
        let mut values = parent_values.clone();
        let mut addresses = parent_addresses.clone();
        self.verify_value_ops(
            root,
            &block.operations,
            param_types,
            local_slots,
            &mut values,
            &mut addresses,
        )?;
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

fn insert_address(
    addresses: &mut BTreeMap<String, String>,
    id: &str,
    type_hash: &str,
) -> Result<()> {
    if addresses
        .insert(id.to_string(), type_hash.to_string())
        .is_some()
    {
        bail!("duplicate lowered address id {id}");
    }
    Ok(())
}

fn value_type<'a>(values: &'a BTreeMap<String, String>, id: &str) -> Result<&'a String> {
    values
        .get(id)
        .ok_or_else(|| anyhow!("unknown lowered value id {id}"))
}

fn address_type<'a>(addresses: &'a BTreeMap<String, String>, id: &str) -> Result<&'a String> {
    addresses
        .get(id)
        .ok_or_else(|| anyhow!("unknown lowered address id {id}"))
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
        | LoweredOp::If { id, .. }
        | LoweredOp::BorrowShared { id, .. }
        | LoweredOp::BorrowMut { id, .. }
        | LoweredOp::DerefShared { id, .. }
        | LoweredOp::DerefMut { id, .. }
        | LoweredOp::AddrOfParam { id, .. }
        | LoweredOp::AddrOfLocal { id, .. }
        | LoweredOp::AddrOfField { id, .. }
        | LoweredOp::AddrOfIndex { id, .. }
        | LoweredOp::Load { id, .. }
        | LoweredOp::Copy { id, .. }
        | LoweredOp::Move { id, .. } => Some(id),
        LoweredOp::Store { .. }
        | LoweredOp::Drop { .. }
        | LoweredOp::BorrowDebug { .. }
        | LoweredOp::Return { .. } => None,
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
        LoweredOp::BorrowShared { .. } => "borrow_shared",
        LoweredOp::BorrowMut { .. } => "borrow_mut",
        LoweredOp::DerefShared { .. } => "deref_shared",
        LoweredOp::DerefMut { .. } => "deref_mut",
        LoweredOp::AddrOfParam { .. } => "addr_of_param",
        LoweredOp::AddrOfLocal { .. } => "addr_of_local",
        LoweredOp::AddrOfField { .. } => "addr_of_field",
        LoweredOp::AddrOfIndex { .. } => "addr_of_index",
        LoweredOp::Load { .. } => "load",
        LoweredOp::Store { .. } => "store",
        LoweredOp::Copy { .. } => "copy",
        LoweredOp::Move { .. } => "move",
        LoweredOp::Drop { .. } => "drop",
        LoweredOp::BorrowDebug { .. } => "borrow_debug",
        LoweredOp::Return { .. } => "return",
    }
}

fn default_slot_size_bytes() -> u64 {
    8
}

fn is_default_slot_size(value: &u64) -> bool {
    *value == default_slot_size_bytes()
}

fn stack_slot_size_bytes(layout_size_bytes: u64) -> u64 {
    layout_size_bytes.max(1).div_ceil(8) * 8
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
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
