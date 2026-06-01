//! Native object backends for the v0 lowered IR targets.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value as JsonValue, json};

use crate::abi::internal_abi_symbol;
use crate::artifact::CacheKeyInput;
use crate::backend::{ArtifactKind, ObjectBackend, ObjectBackendArtifact, ObjectBackendInput};
use crate::lowering::{LoweredBlock, LoweredFunctionIr, LoweredOp};
use crate::model::ProgramRootPayload;
use crate::store::{
    CodeDb, cache_key_for_input, canonical_json, function_interface_metadata, hash_bytes,
};
use crate::types::type_hash_for;
use crate::{APPLE_ARM64_TARGET, BYTES_DOMAIN, LINUX_X86_64_TARGET, MAIN_BRANCH};

pub(crate) const ELF_BACKEND_ID: &str = "native-elf-x86_64-v0";
pub(crate) const MACHO_BACKEND_ID: &str = "native-macho-arm64-v0";
const OBJECT_METADATA_SCHEMA: &str = "codedb/native-object/v1";
const ELF_OBJECT_FORMAT: &str = "elf64-x86-64-relocatable";
const MACHO_OBJECT_FORMAT: &str = "macho64-arm64-relocatable";
const R_X86_64_PLT32: u32 = 4;
const ARM64_RELOC_BRANCH26: u32 = 2;

pub(crate) struct ElfObjectBackend;
pub(crate) struct MachOArm64ObjectBackend;

pub(crate) struct NativeObjectArtifact {
    pub(crate) artifact_hash: String,
    pub(crate) cache_key: String,
    pub(crate) metadata: JsonValue,
    pub(crate) bytes: Vec<u8>,
}

impl ObjectBackend for ElfObjectBackend {
    fn backend_id(&self) -> &'static str {
        ELF_BACKEND_ID
    }

    fn emit_object(&self, input: ObjectBackendInput<'_>) -> Result<ObjectBackendArtifact> {
        if input.target_triple != LINUX_X86_64_TARGET {
            bail!("{ELF_BACKEND_ID} only supports target {LINUX_X86_64_TARGET}");
        }

        validate_native_ir(input.ir)?;
        let function_symbol = internal_abi_symbol(&input.ir.symbol_hash)?;
        let compiled = compile_x86_64_function(input.ir, &function_symbol)?;
        let bytes = write_elf_object(&function_symbol, &compiled.text, &compiled.relocations);
        let artifact_hash = hash_bytes(BYTES_DOMAIN, &bytes);
        let relocations = compiled
            .relocations
            .iter()
            .map(|relocation| {
                json!({
                    "offset": relocation.offset,
                    "kind": "R_X86_64_PLT32",
                    "target_symbol_hash": &relocation.target_symbol_hash,
                    "target_abi_symbol": &relocation.target_abi_symbol,
                })
            })
            .collect::<Vec<_>>();
        let called_symbols = compiled
            .relocations
            .iter()
            .map(|relocation| relocation.target_symbol_hash.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let metadata = json!({
            "schema": OBJECT_METADATA_SCHEMA,
            "backend_id": ELF_BACKEND_ID,
            "object_format": ELF_OBJECT_FORMAT,
            "target_triple": input.target_triple,
            "symbol_hash": &input.ir.symbol_hash,
            "function_def_hash": &input.ir.function_def_hash,
            "function_sig_hash": &input.ir.function_sig_hash,
            "typed_body_expr_hash": &input.ir.typed_body_expr_hash,
            "lowered_ir_schema": &input.ir.schema,
            "defined_symbols": [function_symbol],
            "called_symbols": called_symbols,
            "relocations": relocations,
        });

        Ok(ObjectBackendArtifact {
            artifact_hash,
            metadata,
            bytes,
        })
    }
}

impl ObjectBackend for MachOArm64ObjectBackend {
    fn backend_id(&self) -> &'static str {
        MACHO_BACKEND_ID
    }

    fn emit_object(&self, input: ObjectBackendInput<'_>) -> Result<ObjectBackendArtifact> {
        if input.target_triple != APPLE_ARM64_TARGET {
            bail!("{MACHO_BACKEND_ID} only supports target {APPLE_ARM64_TARGET}");
        }

        validate_native_ir(input.ir)?;
        let function_symbol = internal_abi_symbol(&input.ir.symbol_hash)?;
        let object_symbol = macho_symbol_name(&function_symbol);
        let compiled = compile_arm64_function(input.ir, &object_symbol)?;
        let bytes = write_macho_object(&object_symbol, &compiled.text, &compiled.relocations);
        let artifact_hash = hash_bytes(BYTES_DOMAIN, &bytes);
        let relocations = compiled
            .relocations
            .iter()
            .map(|relocation| {
                json!({
                    "offset": relocation.offset,
                    "kind": "ARM64_RELOC_BRANCH26",
                    "target_symbol_hash": &relocation.target_symbol_hash,
                    "target_abi_symbol": strip_macho_symbol_prefix(&relocation.target_abi_symbol),
                    "target_object_symbol": &relocation.target_abi_symbol,
                })
            })
            .collect::<Vec<_>>();
        let called_symbols = compiled
            .relocations
            .iter()
            .map(|relocation| relocation.target_symbol_hash.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let metadata = json!({
            "schema": OBJECT_METADATA_SCHEMA,
            "backend_id": MACHO_BACKEND_ID,
            "object_format": MACHO_OBJECT_FORMAT,
            "target_triple": input.target_triple,
            "symbol_hash": &input.ir.symbol_hash,
            "function_def_hash": &input.ir.function_def_hash,
            "function_sig_hash": &input.ir.function_sig_hash,
            "typed_body_expr_hash": &input.ir.typed_body_expr_hash,
            "lowered_ir_schema": &input.ir.schema,
            "defined_symbols": [function_symbol],
            "object_symbols": [object_symbol],
            "called_symbols": called_symbols,
            "relocations": relocations,
        });

        Ok(ObjectBackendArtifact {
            artifact_hash,
            metadata,
            bytes,
        })
    }
}

impl CodeDb {
    pub fn emit_object_main_branch(
        &mut self,
        function_name: &str,
        target_triple: &str,
    ) -> Result<Vec<u8>> {
        let artifact = self.emit_object_main_branch_artifact(function_name, target_triple)?;
        debug_assert!(artifact.artifact_hash.starts_with("sha256:"));
        debug_assert!(artifact.metadata.is_object());
        Ok(artifact.bytes)
    }

    pub(crate) fn emit_object_main_branch_artifact(
        &mut self,
        function_name: &str,
        target_triple: &str,
    ) -> Result<NativeObjectArtifact> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self
            .resolve_name(&branch.root_hash, "main", function_name)
            .map_err(|err| anyhow!("unknown entry function {function_name}: {err}"))?;
        self.emit_object_for_symbol(&branch.root_hash, &symbol, target_triple)
    }

    pub(crate) fn emit_object_for_symbol(
        &mut self,
        root_hash: &str,
        symbol: &str,
        target_triple: &str,
    ) -> Result<NativeObjectArtifact> {
        let root = self.load_root(root_hash)?;
        let lowered = self.lower_symbol(root_hash, symbol)?;
        let dependency_interface_hashes = self.dependency_interface_hashes(&root, &lowered.ir)?;
        let dependency_closure = self.dependency_closure_for_symbol(root_hash, symbol)?;
        let backend_id = backend_id_for_target(target_triple)?;
        let key_input = CacheKeyInput::new(
            ArtifactKind::ObjectFile,
            &lowered.ir.function_def_hash,
            backend_id,
            target_triple,
        )
        .with_dependency_interface_hashes(dependency_interface_hashes.clone());
        let object_cache_key = cache_key_for_input(&key_input)?;

        if let Some(cache_entry) = self.lookup_cache(&key_input)? {
            let bytes = cache_entry
                .artifact_bytes
                .ok_or_else(|| anyhow!("object cache entry missing artifact_bytes"))?;
            let metadata = cache_entry
                .artifact_json
                .ok_or_else(|| anyhow!("object cache entry missing artifact_json"))?;
            let metadata = object_metadata_from_cache(&metadata)?;
            return Ok(NativeObjectArtifact {
                artifact_hash: cache_entry.artifact_hash,
                cache_key: cache_entry.cache_key,
                metadata,
                bytes,
            });
        }

        let input = ObjectBackendInput {
            ir: &lowered.ir,
            target_triple,
        };
        let emitted = match target_triple {
            LINUX_X86_64_TARGET => ElfObjectBackend.emit_object(input)?,
            APPLE_ARM64_TARGET => MachOArm64ObjectBackend.emit_object(input)?,
            _ => unreachable!("unsupported target was checked by backend_id_for_target"),
        };
        let mut metadata = emitted.metadata;
        add_native_object_dependency_metadata(
            &mut metadata,
            &dependency_interface_hashes,
            &dependency_closure,
        )?;
        self.write_cache_bytes(key_input, &metadata, &emitted.bytes)?;
        Ok(NativeObjectArtifact {
            artifact_hash: emitted.artifact_hash,
            cache_key: object_cache_key,
            metadata,
            bytes: emitted.bytes,
        })
    }

    fn dependency_interface_hashes(
        &self,
        root: &ProgramRootPayload,
        ir: &LoweredFunctionIr,
    ) -> Result<Vec<String>> {
        let mut called_symbols = BTreeSet::new();
        collect_called_symbols(&ir.operations, &mut called_symbols);
        called_symbols
            .into_iter()
            .map(|symbol| {
                let entry = self
                    .root_symbol(root, &symbol)
                    .ok_or_else(|| anyhow!("native object dependency missing {symbol}"))?;
                let metadata = function_interface_metadata(&entry.symbol, &entry.signature)?;
                Ok(hash_bytes(
                    BYTES_DOMAIN,
                    canonical_json(&metadata).as_bytes(),
                ))
            })
            .collect()
    }

    fn dependency_closure_for_symbol(&self, root_hash: &str, symbol: &str) -> Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        self.collect_dependency_closure(root_hash, symbol, symbol, &mut seen)?;
        Ok(seen.into_iter().collect())
    }

    fn collect_dependency_closure(
        &self,
        root_hash: &str,
        origin: &str,
        symbol: &str,
        seen: &mut BTreeSet<String>,
    ) -> Result<()> {
        for dependency in self.dependencies_for_symbol(root_hash, symbol)? {
            if dependency == origin {
                continue;
            }
            if seen.insert(dependency.clone()) {
                self.collect_dependency_closure(root_hash, origin, &dependency, seen)?;
            }
        }
        Ok(())
    }
}

fn add_native_object_dependency_metadata(
    metadata: &mut JsonValue,
    dependency_interface_hashes: &[String],
    dependency_closure: &[String],
) -> Result<()> {
    let object = metadata
        .as_object_mut()
        .ok_or_else(|| anyhow!("native object metadata must be a JSON object"))?;
    object.insert(
        "dependency_interface_hashes".to_string(),
        json!(dependency_interface_hashes),
    );
    object.insert("dependency_closure".to_string(), json!(dependency_closure));
    Ok(())
}

pub(crate) fn object_metadata_from_cache(artifact_json: &JsonValue) -> Result<JsonValue> {
    if artifact_json
        .get("content_kind")
        .and_then(JsonValue::as_str)
        == Some("bytes")
    {
        return artifact_json
            .get("metadata")
            .cloned()
            .ok_or_else(|| anyhow!("object cache entry missing metadata"));
    }
    Ok(artifact_json.clone())
}

fn backend_id_for_target(target_triple: &str) -> Result<&'static str> {
    match target_triple {
        LINUX_X86_64_TARGET => Ok(ElfObjectBackend.backend_id()),
        APPLE_ARM64_TARGET => Ok(MachOArm64ObjectBackend.backend_id()),
        other => bail!(
            "unsupported native object target {other}; supported targets: {LINUX_X86_64_TARGET}, {APPLE_ARM64_TARGET}"
        ),
    }
}

fn collect_called_symbols(operations: &[LoweredOp], out: &mut BTreeSet<String>) {
    for op in operations {
        match op {
            LoweredOp::Call {
                target_symbol_hash, ..
            } => {
                out.insert(target_symbol_hash.clone());
            }
            LoweredOp::If {
                then_block,
                else_block,
                ..
            } => {
                collect_called_symbols(&then_block.operations, out);
                collect_called_symbols(&else_block.operations, out);
            }
            LoweredOp::Param { .. }
            | LoweredOp::ConstI64 { .. }
            | LoweredOp::ConstBool { .. }
            | LoweredOp::Binary { .. }
            | LoweredOp::Return { .. } => {}
        }
    }
}

fn validate_native_ir(ir: &LoweredFunctionIr) -> Result<()> {
    let i64_type = type_hash_for("I64");
    let bool_type = type_hash_for("Bool");
    if ir.return_type_hash != i64_type && ir.return_type_hash != bool_type {
        bail!("native object backend v0 supports only i64 and bool returns");
    }
    if ir.params.len() > 6 {
        bail!("native object backend v0 supports at most 6 parameters");
    }
    for param in &ir.params {
        if param.type_hash != i64_type && param.type_hash != bool_type {
            bail!("native object backend v0 supports only i64 and bool parameters");
        }
    }
    validate_native_ops(&ir.operations, &i64_type, &bool_type)
}

fn validate_native_ops(operations: &[LoweredOp], i64_type: &str, bool_type: &str) -> Result<()> {
    for op in operations {
        match op {
            LoweredOp::Param { type_hash, .. }
            | LoweredOp::ConstI64 { type_hash, .. }
            | LoweredOp::ConstBool { type_hash, .. }
            | LoweredOp::Binary { type_hash, .. }
            | LoweredOp::Call { type_hash, .. }
            | LoweredOp::If { type_hash, .. }
            | LoweredOp::Return { type_hash, .. } => {
                if type_hash != i64_type && type_hash != bool_type {
                    bail!("native object backend v0 supports only i64 and bool values");
                }
            }
        }
        if let LoweredOp::If {
            then_block,
            else_block,
            ..
        } = op
        {
            validate_native_ops(&then_block.operations, i64_type, bool_type)?;
            validate_native_ops(&else_block.operations, i64_type, bool_type)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct CompiledFunction {
    text: Vec<u8>,
    relocations: Vec<TextRelocation>,
}

#[derive(Debug, Clone)]
struct TextRelocation {
    offset: u64,
    target_symbol_hash: String,
    target_abi_symbol: String,
}

#[derive(Debug)]
struct StackLayout {
    param_offsets: Vec<i32>,
    value_offsets: BTreeMap<String, i32>,
    stack_size: i32,
}

fn compile_x86_64_function(
    ir: &LoweredFunctionIr,
    function_symbol: &str,
) -> Result<CompiledFunction> {
    let layout = StackLayout::new(ir)?;
    let mut emitter = FunctionEmitter {
        layout,
        text: Vec::new(),
        relocations: Vec::new(),
    };

    emitter.emit_prologue(ir.params.len())?;
    let (last, body) = ir
        .operations
        .split_last()
        .ok_or_else(|| anyhow!("lowered function has no return"))?;
    emitter.emit_ops(body)?;
    match last {
        LoweredOp::Return { value, .. } => {
            let offset = emitter.value_offset(value)?;
            emitter.mov_rax_stack(offset);
            emitter.emit_epilogue();
        }
        _ => bail!("lowered function must end with return"),
    }

    if function_symbol.is_empty() {
        bail!("native object function symbol is empty");
    }

    Ok(CompiledFunction {
        text: emitter.text,
        relocations: emitter.relocations,
    })
}

impl StackLayout {
    fn new(ir: &LoweredFunctionIr) -> Result<Self> {
        let mut ids = Vec::new();
        collect_value_ids(&ir.operations, &mut ids)?;
        let mut value_offsets = BTreeMap::new();
        let mut next_slot = ir.params.len();
        for id in ids {
            let offset = -8 * (next_slot as i32 + 1);
            value_offsets.insert(id, offset);
            next_slot += 1;
        }
        let param_offsets = (0..ir.params.len())
            .map(|idx| -8 * (idx as i32 + 1))
            .collect::<Vec<_>>();
        let raw_size = next_slot as i32 * 8;
        let stack_size = if raw_size == 0 {
            0
        } else {
            ((raw_size + 15) / 16) * 16
        };
        Ok(Self {
            param_offsets,
            value_offsets,
            stack_size,
        })
    }
}

fn collect_value_ids(operations: &[LoweredOp], ids: &mut Vec<String>) -> Result<()> {
    let mut seen = ids.iter().cloned().collect::<BTreeSet<_>>();
    collect_value_ids_inner(operations, ids, &mut seen)
}

fn collect_value_ids_inner(
    operations: &[LoweredOp],
    ids: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    for op in operations {
        match op {
            LoweredOp::Param { id, .. }
            | LoweredOp::ConstI64 { id, .. }
            | LoweredOp::ConstBool { id, .. }
            | LoweredOp::Binary { id, .. }
            | LoweredOp::Call { id, .. } => push_value_id(ids, seen, id)?,
            LoweredOp::If {
                id,
                then_block,
                else_block,
                ..
            } => {
                push_value_id(ids, seen, id)?;
                collect_value_ids_inner(&then_block.operations, ids, seen)?;
                collect_value_ids_inner(&else_block.operations, ids, seen)?;
            }
            LoweredOp::Return { .. } => {}
        }
    }
    Ok(())
}

fn push_value_id(ids: &mut Vec<String>, seen: &mut BTreeSet<String>, id: &str) -> Result<()> {
    if !seen.insert(id.to_string()) {
        bail!("duplicate lowered value id {id}");
    }
    ids.push(id.to_string());
    Ok(())
}

struct FunctionEmitter {
    layout: StackLayout,
    text: Vec<u8>,
    relocations: Vec<TextRelocation>,
}

impl FunctionEmitter {
    fn emit_prologue(&mut self, param_count: usize) -> Result<()> {
        self.text.push(0x55);
        self.text.extend_from_slice(&[0x48, 0x89, 0xe5]);
        if self.layout.stack_size > 0 {
            if self.layout.stack_size <= i8::MAX as i32 {
                self.text.extend_from_slice(&[0x48, 0x83, 0xec]);
                self.text.push(self.layout.stack_size as u8);
            } else {
                self.text.extend_from_slice(&[0x48, 0x81, 0xec]);
                self.push_i32(self.layout.stack_size);
            }
        }
        for slot in 0..param_count {
            self.mov_stack_arg_reg(self.layout.param_offsets[slot], slot)?;
        }
        Ok(())
    }

    fn emit_epilogue(&mut self) {
        self.text.extend_from_slice(&[0xc9, 0xc3]);
    }

    fn emit_ops(&mut self, operations: &[LoweredOp]) -> Result<()> {
        for op in operations {
            self.emit_op(op)?;
        }
        Ok(())
    }

    fn emit_op(&mut self, op: &LoweredOp) -> Result<()> {
        match op {
            LoweredOp::Param { id, slot, .. } => {
                let param = *self
                    .layout
                    .param_offsets
                    .get(*slot)
                    .ok_or_else(|| anyhow!("parameter slot out of bounds {slot}"))?;
                let value = self.value_offset(id)?;
                self.mov_rax_stack(param);
                self.mov_stack_rax(value);
            }
            LoweredOp::ConstI64 { id, value, .. } => {
                let value = value.parse::<i64>()?;
                self.mov_rax_imm64(value);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::ConstBool { id, value, .. } => {
                self.mov_rax_imm32(if *value { 1 } else { 0 });
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::Binary {
                id,
                kind,
                left,
                right,
                ..
            } => {
                self.emit_binary(kind, left, right)?;
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::Call {
                id,
                target_symbol_hash,
                args,
                ..
            } => {
                if args.len() > 6 {
                    bail!("native object backend v0 supports at most 6 call arguments");
                }
                for (idx, arg) in args.iter().enumerate() {
                    self.mov_arg_reg_stack(idx, self.value_offset(arg)?)?;
                }
                let target_abi_symbol = internal_abi_symbol(target_symbol_hash)?;
                let offset = self.text.len() + 1;
                self.text.push(0xe8);
                self.text.extend_from_slice(&[0, 0, 0, 0]);
                self.relocations.push(TextRelocation {
                    offset: offset as u64,
                    target_symbol_hash: target_symbol_hash.clone(),
                    target_abi_symbol,
                });
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::If {
                id,
                cond,
                then_block,
                else_block,
                ..
            } => {
                self.emit_if(id, cond, then_block, else_block)?;
            }
            LoweredOp::Return { .. } => {
                bail!("return is only valid as the final lowered operation");
            }
        }
        Ok(())
    }

    fn emit_binary(&mut self, kind: &str, left: &str, right: &str) -> Result<()> {
        self.mov_rax_stack(self.value_offset(left)?);
        self.mov_rcx_stack(self.value_offset(right)?);
        match kind {
            "add_i64" => self.text.extend_from_slice(&[0x48, 0x01, 0xc8]),
            "sub_i64" => self.text.extend_from_slice(&[0x48, 0x29, 0xc8]),
            "mul_i64" => self.text.extend_from_slice(&[0x48, 0x0f, 0xaf, 0xc1]),
            "div_i64" => {
                self.text.extend_from_slice(&[0x48, 0x85, 0xc9]);
                self.text.extend_from_slice(&[0x75, 0x02]);
                self.text.extend_from_slice(&[0x0f, 0x0b]);
                self.text.extend_from_slice(&[0x48, 0x99]);
                self.text.extend_from_slice(&[0x48, 0xf7, 0xf9]);
            }
            "eq_i64" | "ne_i64" | "lt_i64" | "le_i64" | "gt_i64" | "ge_i64" => {
                self.text.extend_from_slice(&[0x48, 0x39, 0xc8]);
                let cc = match kind {
                    "eq_i64" => 0x94,
                    "ne_i64" => 0x95,
                    "lt_i64" => 0x9c,
                    "le_i64" => 0x9e,
                    "gt_i64" => 0x9f,
                    "ge_i64" => 0x9d,
                    _ => unreachable!(),
                };
                self.text.extend_from_slice(&[0x0f, cc, 0xc0]);
                self.text.extend_from_slice(&[0x0f, 0xb6, 0xc0]);
            }
            "and_bool" => self.text.extend_from_slice(&[0x48, 0x21, 0xc8]),
            "or_bool" => self.text.extend_from_slice(&[0x48, 0x09, 0xc8]),
            other => bail!("unsupported lowered binary op for native object backend: {other}"),
        }
        Ok(())
    }

    fn emit_if(
        &mut self,
        id: &str,
        cond: &str,
        then_block: &LoweredBlock,
        else_block: &LoweredBlock,
    ) -> Result<()> {
        self.cmp_stack_imm8(self.value_offset(cond)?, 0);
        let else_patch = self.emit_jz_placeholder();
        self.emit_ops(&then_block.operations)?;
        self.mov_rax_stack(self.value_offset(&then_block.result)?);
        let end_patch = self.emit_jmp_placeholder();
        self.patch_rel32(else_patch)?;
        self.emit_ops(&else_block.operations)?;
        self.mov_rax_stack(self.value_offset(&else_block.result)?);
        self.patch_rel32(end_patch)?;
        self.mov_stack_rax(self.value_offset(id)?);
        Ok(())
    }

    fn value_offset(&self, id: &str) -> Result<i32> {
        self.layout
            .value_offsets
            .get(id)
            .copied()
            .ok_or_else(|| anyhow!("unknown lowered value id {id}"))
    }

    fn mov_rax_imm64(&mut self, value: i64) {
        self.text.extend_from_slice(&[0x48, 0xb8]);
        self.text.extend_from_slice(&(value as u64).to_le_bytes());
    }

    fn mov_rax_imm32(&mut self, value: i32) {
        self.text.push(0xb8);
        self.push_i32(value);
    }

    fn mov_rax_stack(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x85]);
        self.push_i32(offset);
    }

    fn mov_rcx_stack(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x8d]);
        self.push_i32(offset);
    }

    fn mov_stack_rax(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x89, 0x85]);
        self.push_i32(offset);
    }

    fn mov_stack_arg_reg(&mut self, offset: i32, arg_idx: usize) -> Result<()> {
        let (rex, modrm) = match arg_idx {
            0 => (0x48, 0xbd),
            1 => (0x48, 0xb5),
            2 => (0x48, 0x95),
            3 => (0x48, 0x8d),
            4 => (0x4c, 0x85),
            5 => (0x4c, 0x8d),
            _ => bail!("native object backend v0 supports at most 6 parameters"),
        };
        self.text.extend_from_slice(&[rex, 0x89, modrm]);
        self.push_i32(offset);
        Ok(())
    }

    fn mov_arg_reg_stack(&mut self, arg_idx: usize, offset: i32) -> Result<()> {
        let (rex, modrm) = match arg_idx {
            0 => (0x48, 0xbd),
            1 => (0x48, 0xb5),
            2 => (0x48, 0x95),
            3 => (0x48, 0x8d),
            4 => (0x4c, 0x85),
            5 => (0x4c, 0x8d),
            _ => bail!("native object backend v0 supports at most 6 call arguments"),
        };
        self.text.extend_from_slice(&[rex, 0x8b, modrm]);
        self.push_i32(offset);
        Ok(())
    }

    fn cmp_stack_imm8(&mut self, offset: i32, value: i8) {
        self.text.extend_from_slice(&[0x48, 0x83, 0xbd]);
        self.push_i32(offset);
        self.text.push(value as u8);
    }

    fn emit_jz_placeholder(&mut self) -> usize {
        self.text.extend_from_slice(&[0x0f, 0x84]);
        let patch = self.text.len();
        self.text.extend_from_slice(&[0, 0, 0, 0]);
        patch
    }

    fn emit_jmp_placeholder(&mut self) -> usize {
        self.text.push(0xe9);
        let patch = self.text.len();
        self.text.extend_from_slice(&[0, 0, 0, 0]);
        patch
    }

    fn patch_rel32(&mut self, patch_offset: usize) -> Result<()> {
        let target = self.text.len() as i64;
        let next = (patch_offset + 4) as i64;
        let disp = target - next;
        let disp = i32::try_from(disp)?;
        self.text[patch_offset..patch_offset + 4].copy_from_slice(&disp.to_le_bytes());
        Ok(())
    }

    fn push_i32(&mut self, value: i32) {
        self.text.extend_from_slice(&value.to_le_bytes());
    }
}

#[derive(Debug)]
struct Arm64StackLayout {
    param_offsets: Vec<u32>,
    value_offsets: BTreeMap<String, u32>,
    stack_size: u32,
}

fn compile_arm64_function(
    ir: &LoweredFunctionIr,
    function_symbol: &str,
) -> Result<CompiledFunction> {
    let layout = Arm64StackLayout::new(ir)?;
    let mut emitter = Arm64Emitter {
        layout,
        text: Vec::new(),
        relocations: Vec::new(),
    };

    emitter.emit_prologue(ir.params.len())?;
    let (last, body) = ir
        .operations
        .split_last()
        .ok_or_else(|| anyhow!("lowered function has no return"))?;
    emitter.emit_ops(body)?;
    match last {
        LoweredOp::Return { value, .. } => {
            let offset = emitter.value_offset(value)?;
            emitter.ldr_stack(0, offset)?;
            emitter.emit_epilogue()?;
        }
        _ => bail!("lowered function must end with return"),
    }

    if function_symbol.is_empty() {
        bail!("native object function symbol is empty");
    }

    Ok(CompiledFunction {
        text: emitter.text,
        relocations: emitter.relocations,
    })
}

impl Arm64StackLayout {
    fn new(ir: &LoweredFunctionIr) -> Result<Self> {
        let mut ids = Vec::new();
        collect_value_ids(&ir.operations, &mut ids)?;
        let mut value_offsets = BTreeMap::new();
        let mut next_slot = ir.params.len();
        for id in ids {
            let offset = 8 * next_slot as u32;
            value_offsets.insert(id, offset);
            next_slot += 1;
        }
        let param_offsets = (0..ir.params.len())
            .map(|idx| 8 * idx as u32)
            .collect::<Vec<_>>();
        let raw_size = next_slot as u32 * 8;
        let stack_size = if raw_size == 0 {
            0
        } else {
            raw_size.div_ceil(16) * 16
        };
        if stack_size > 4095 {
            bail!("native arm64 backend v0 stack frame is too large");
        }
        Ok(Self {
            param_offsets,
            value_offsets,
            stack_size,
        })
    }
}

struct Arm64Emitter {
    layout: Arm64StackLayout,
    text: Vec<u8>,
    relocations: Vec<TextRelocation>,
}

impl Arm64Emitter {
    fn emit_prologue(&mut self, param_count: usize) -> Result<()> {
        self.emit_u32(0xa9bf7bfd);
        self.emit_u32(0x910003fd);
        if self.layout.stack_size > 0 {
            self.sub_sp_imm(self.layout.stack_size)?;
        }
        for slot in 0..param_count {
            self.str_stack(slot as u8, self.layout.param_offsets[slot])?;
        }
        Ok(())
    }

    fn emit_epilogue(&mut self) -> Result<()> {
        if self.layout.stack_size > 0 {
            self.add_sp_imm(self.layout.stack_size)?;
        }
        self.emit_u32(0xa8c17bfd);
        self.emit_u32(0xd65f03c0);
        Ok(())
    }

    fn emit_ops(&mut self, operations: &[LoweredOp]) -> Result<()> {
        for op in operations {
            self.emit_op(op)?;
        }
        Ok(())
    }

    fn emit_op(&mut self, op: &LoweredOp) -> Result<()> {
        match op {
            LoweredOp::Param { id, slot, .. } => {
                let param = *self
                    .layout
                    .param_offsets
                    .get(*slot)
                    .ok_or_else(|| anyhow!("parameter slot out of bounds {slot}"))?;
                self.ldr_stack(0, param)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::ConstI64 { id, value, .. } => {
                self.mov_u64(0, value.parse::<i64>()? as u64);
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::ConstBool { id, value, .. } => {
                self.mov_u64(0, if *value { 1 } else { 0 });
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::Binary {
                id,
                kind,
                left,
                right,
                ..
            } => {
                self.emit_binary(kind, left, right)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::Call {
                id,
                target_symbol_hash,
                args,
                ..
            } => {
                if args.len() > 8 {
                    bail!("native arm64 backend v0 supports at most 8 call arguments");
                }
                for (idx, arg) in args.iter().enumerate() {
                    self.ldr_stack(idx as u8, self.value_offset(arg)?)?;
                }
                let target_abi_symbol =
                    macho_symbol_name(&internal_abi_symbol(target_symbol_hash)?);
                let offset = self.text.len();
                self.emit_u32(0x94000000);
                self.relocations.push(TextRelocation {
                    offset: offset as u64,
                    target_symbol_hash: target_symbol_hash.clone(),
                    target_abi_symbol,
                });
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::If {
                id,
                cond,
                then_block,
                else_block,
                ..
            } => {
                self.emit_if(id, cond, then_block, else_block)?;
            }
            LoweredOp::Return { .. } => {
                bail!("return is only valid as the final lowered operation");
            }
        }
        Ok(())
    }

    fn emit_binary(&mut self, kind: &str, left: &str, right: &str) -> Result<()> {
        self.ldr_stack(0, self.value_offset(left)?)?;
        self.ldr_stack(1, self.value_offset(right)?)?;
        match kind {
            "add_i64" => self.add_reg(0, 0, 1),
            "sub_i64" => self.sub_reg(0, 0, 1),
            "mul_i64" => self.mul_reg(0, 0, 1),
            "div_i64" => {
                let skip_trap = self.emit_cbnz_placeholder(1);
                self.emit_u32(0xd4200000);
                self.patch_imm19(skip_trap)?;
                self.sdiv_reg(0, 0, 1);
            }
            "eq_i64" | "ne_i64" | "lt_i64" | "le_i64" | "gt_i64" | "ge_i64" => {
                self.cmp_reg(0, 1);
                let cond = match kind {
                    "eq_i64" => 0,
                    "ne_i64" => 1,
                    "lt_i64" => 11,
                    "le_i64" => 13,
                    "gt_i64" => 12,
                    "ge_i64" => 10,
                    _ => unreachable!(),
                };
                self.cset(0, cond);
            }
            "and_bool" => self.and_reg(0, 0, 1),
            "or_bool" => self.orr_reg(0, 0, 1),
            other => bail!("unsupported lowered binary op for native arm64 backend: {other}"),
        }
        Ok(())
    }

    fn emit_if(
        &mut self,
        id: &str,
        cond: &str,
        then_block: &LoweredBlock,
        else_block: &LoweredBlock,
    ) -> Result<()> {
        self.ldr_stack(0, self.value_offset(cond)?)?;
        let else_patch = self.emit_cbz_placeholder(0);
        self.emit_ops(&then_block.operations)?;
        self.ldr_stack(0, self.value_offset(&then_block.result)?)?;
        let end_patch = self.emit_b_placeholder();
        self.patch_imm19(else_patch)?;
        self.emit_ops(&else_block.operations)?;
        self.ldr_stack(0, self.value_offset(&else_block.result)?)?;
        self.patch_imm26(end_patch)?;
        self.str_stack(0, self.value_offset(id)?)?;
        Ok(())
    }

    fn value_offset(&self, id: &str) -> Result<u32> {
        self.layout
            .value_offsets
            .get(id)
            .copied()
            .ok_or_else(|| anyhow!("unknown lowered value id {id}"))
    }

    fn sub_sp_imm(&mut self, imm: u32) -> Result<()> {
        if imm > 4095 {
            bail!("arm64 stack adjustment too large");
        }
        self.emit_u32(0xd10003ff | (imm << 10));
        Ok(())
    }

    fn add_sp_imm(&mut self, imm: u32) -> Result<()> {
        if imm > 4095 {
            bail!("arm64 stack adjustment too large");
        }
        self.emit_u32(0x910003ff | (imm << 10));
        Ok(())
    }

    fn str_stack(&mut self, reg: u8, offset: u32) -> Result<()> {
        self.stack_mem_op(0xf90003e0, reg, offset)
    }

    fn ldr_stack(&mut self, reg: u8, offset: u32) -> Result<()> {
        self.stack_mem_op(0xf94003e0, reg, offset)
    }

    fn stack_mem_op(&mut self, base: u32, reg: u8, offset: u32) -> Result<()> {
        if reg > 30 {
            bail!("invalid arm64 general register x{reg}");
        }
        if !offset.is_multiple_of(8) || offset / 8 > 4095 {
            bail!("arm64 stack offset cannot be encoded");
        }
        self.emit_u32(base | ((offset / 8) << 10) | u32::from(reg));
        Ok(())
    }

    fn mov_u64(&mut self, reg: u8, value: u64) {
        let chunk0 = (value & 0xffff) as u32;
        self.emit_u32(0xd2800000 | (chunk0 << 5) | u32::from(reg));
        for hw in 1..4 {
            let chunk = ((value >> (hw * 16)) & 0xffff) as u32;
            if chunk != 0 {
                self.emit_u32(0xf2800000 | ((hw as u32) << 21) | (chunk << 5) | u32::from(reg));
            }
        }
    }

    fn add_reg(&mut self, rd: u8, rn: u8, rm: u8) {
        self.emit_u32(0x8b000000 | (u32::from(rm) << 16) | (u32::from(rn) << 5) | u32::from(rd));
    }

    fn sub_reg(&mut self, rd: u8, rn: u8, rm: u8) {
        self.emit_u32(0xcb000000 | (u32::from(rm) << 16) | (u32::from(rn) << 5) | u32::from(rd));
    }

    fn mul_reg(&mut self, rd: u8, rn: u8, rm: u8) {
        self.emit_u32(
            0x9b000000 | (u32::from(rm) << 16) | (31 << 10) | (u32::from(rn) << 5) | u32::from(rd),
        );
    }

    fn sdiv_reg(&mut self, rd: u8, rn: u8, rm: u8) {
        self.emit_u32(0x9ac00c00 | (u32::from(rm) << 16) | (u32::from(rn) << 5) | u32::from(rd));
    }

    fn cmp_reg(&mut self, rn: u8, rm: u8) {
        self.emit_u32(0xeb00001f | (u32::from(rm) << 16) | (u32::from(rn) << 5));
    }

    fn cset(&mut self, rd: u8, cond: u8) {
        let inverted = u32::from(cond ^ 1);
        self.emit_u32(0x9a9f07e0 | (inverted << 12) | u32::from(rd));
    }

    fn and_reg(&mut self, rd: u8, rn: u8, rm: u8) {
        self.emit_u32(0x8a000000 | (u32::from(rm) << 16) | (u32::from(rn) << 5) | u32::from(rd));
    }

    fn orr_reg(&mut self, rd: u8, rn: u8, rm: u8) {
        self.emit_u32(0xaa000000 | (u32::from(rm) << 16) | (u32::from(rn) << 5) | u32::from(rd));
    }

    fn emit_cbz_placeholder(&mut self, reg: u8) -> usize {
        let patch = self.text.len();
        self.emit_u32(0xb4000000 | u32::from(reg));
        patch
    }

    fn emit_cbnz_placeholder(&mut self, reg: u8) -> usize {
        let patch = self.text.len();
        self.emit_u32(0xb5000000 | u32::from(reg));
        patch
    }

    fn emit_b_placeholder(&mut self) -> usize {
        let patch = self.text.len();
        self.emit_u32(0x14000000);
        patch
    }

    fn patch_imm19(&mut self, patch_offset: usize) -> Result<()> {
        let disp = self.branch_disp_words(patch_offset)?;
        if !(-(1 << 18)..(1 << 18)).contains(&disp) {
            bail!("arm64 conditional branch target out of range");
        }
        let encoded = (disp as i32 as u32) & 0x7ffff;
        let mut instruction = u32::from_le_bytes(
            self.text[patch_offset..patch_offset + 4]
                .try_into()
                .expect("instruction bytes"),
        );
        instruction = (instruction & !(0x7ffff << 5)) | (encoded << 5);
        self.text[patch_offset..patch_offset + 4].copy_from_slice(&instruction.to_le_bytes());
        Ok(())
    }

    fn patch_imm26(&mut self, patch_offset: usize) -> Result<()> {
        let disp = self.branch_disp_words(patch_offset)?;
        if !(-(1 << 25)..(1 << 25)).contains(&disp) {
            bail!("arm64 branch target out of range");
        }
        let encoded = (disp as i32 as u32) & 0x03ff_ffff;
        let mut instruction = u32::from_le_bytes(
            self.text[patch_offset..patch_offset + 4]
                .try_into()
                .expect("instruction bytes"),
        );
        instruction = (instruction & !0x03ff_ffff) | encoded;
        self.text[patch_offset..patch_offset + 4].copy_from_slice(&instruction.to_le_bytes());
        Ok(())
    }

    fn branch_disp_words(&self, patch_offset: usize) -> Result<i64> {
        let target = self.text.len() as i64;
        let source = patch_offset as i64;
        let bytes = target - source;
        if bytes % 4 != 0 {
            bail!("arm64 branch target is not instruction-aligned");
        }
        Ok(bytes / 4)
    }

    fn emit_u32(&mut self, value: u32) {
        self.text.extend_from_slice(&value.to_le_bytes());
    }
}

#[derive(Debug, Clone)]
struct Section {
    name: &'static str,
    section_type: u32,
    flags: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
    data: Vec<u8>,
}

fn write_elf_object(function_symbol: &str, text: &[u8], relocations: &[TextRelocation]) -> Vec<u8> {
    let mut strtab = StringTable::default();
    let function_name = strtab.insert(function_symbol);
    let external_symbols = relocations
        .iter()
        .map(|relocation| relocation.target_abi_symbol.clone())
        .filter(|symbol| symbol != function_symbol)
        .collect::<BTreeSet<_>>();
    let mut symbols = Vec::new();
    symbols.push(SymbolEntry::null());
    symbols.push(SymbolEntry {
        name: 0,
        info: 3,
        shndx: 1,
        value: 0,
        size: 0,
    });
    symbols.push(SymbolEntry {
        name: function_name,
        info: 0x12,
        shndx: 1,
        value: 0,
        size: text.len() as u64,
    });
    let mut symbol_indexes = BTreeMap::new();
    symbol_indexes.insert(function_symbol.to_string(), 2_u32);
    for external in &external_symbols {
        let name = strtab.insert(external);
        let idx = symbols.len() as u32;
        symbols.push(SymbolEntry {
            name,
            info: 0x12,
            shndx: 0,
            value: 0,
            size: 0,
        });
        symbol_indexes.insert(external.clone(), idx);
    }
    let symtab = symbols
        .iter()
        .flat_map(SymbolEntry::to_bytes)
        .collect::<Vec<_>>();
    let rela = relocations
        .iter()
        .flat_map(|relocation| {
            let symbol_index = symbol_indexes[&relocation.target_abi_symbol];
            let info = ((symbol_index as u64) << 32) | R_X86_64_PLT32 as u64;
            rela_entry(relocation.offset, info, -4)
        })
        .collect::<Vec<_>>();

    let sections = vec![
        Section {
            name: ".text",
            section_type: 1,
            flags: 0x6,
            link: 0,
            info: 0,
            align: 16,
            entsize: 0,
            data: text.to_vec(),
        },
        Section {
            name: ".rela.text",
            section_type: 4,
            flags: 0x40,
            link: 3,
            info: 1,
            align: 8,
            entsize: 24,
            data: rela,
        },
        Section {
            name: ".symtab",
            section_type: 2,
            flags: 0,
            link: 4,
            info: 2,
            align: 8,
            entsize: 24,
            data: symtab,
        },
        Section {
            name: ".strtab",
            section_type: 3,
            flags: 0,
            link: 0,
            info: 0,
            align: 1,
            entsize: 0,
            data: strtab.bytes,
        },
        Section {
            name: ".shstrtab",
            section_type: 3,
            flags: 0,
            link: 0,
            info: 0,
            align: 1,
            entsize: 0,
            data: section_name_table().bytes,
        },
    ];

    let mut offset = 64_u64;
    let mut section_offsets = Vec::new();
    for section in &sections {
        offset = align_to(offset, section.align);
        section_offsets.push(offset);
        offset += section.data.len() as u64;
    }
    let section_header_offset = align_to(offset, 8);
    let section_count = sections.len() as u16 + 1;

    let mut out = elf_header(section_header_offset, section_count, sections.len() as u16);
    for (section, section_offset) in sections.iter().zip(section_offsets.iter()) {
        pad_to(&mut out, *section_offset as usize);
        out.extend_from_slice(&section.data);
    }
    pad_to(&mut out, section_header_offset as usize);

    out.extend_from_slice(&[0; 64]);
    let shstrtab = section_name_table();
    for (idx, section) in sections.iter().enumerate() {
        out.extend_from_slice(&section_header(
            shstrtab.offset(section.name),
            section.section_type,
            section.flags,
            section_offsets[idx],
            section.data.len() as u64,
            section.link,
            section.info,
            section.align,
            section.entsize,
        ));
    }
    out
}

fn write_macho_object(
    function_symbol: &str,
    text: &[u8],
    relocations: &[TextRelocation],
) -> Vec<u8> {
    let mut strtab = StringTable::default();
    let function_name = strtab.insert(function_symbol);
    let external_symbols = relocations
        .iter()
        .map(|relocation| relocation.target_abi_symbol.clone())
        .filter(|symbol| symbol != function_symbol)
        .collect::<BTreeSet<_>>();

    let mut symbols = Vec::new();
    let mut symbol_indexes = BTreeMap::new();
    symbols.push(MachOSymbolEntry {
        name: function_name,
        ty: 0x0f,
        sect: 1,
        desc: 0,
        value: 0,
    });
    symbol_indexes.insert(function_symbol.to_string(), 0_u32);
    for external in &external_symbols {
        let name = strtab.insert(external);
        let idx = symbols.len() as u32;
        symbols.push(MachOSymbolEntry {
            name,
            ty: 0x01,
            sect: 0,
            desc: 0,
            value: 0,
        });
        symbol_indexes.insert(external.clone(), idx);
    }

    const HEADER_SIZE: u64 = 32;
    const SIZEOF_CMDS: u32 = 280;
    let text_offset = HEADER_SIZE + u64::from(SIZEOF_CMDS);
    let text_end = text_offset + text.len() as u64;
    let reloc_offset = if relocations.is_empty() {
        0
    } else {
        align_to(text_end, 4)
    };
    let reloc_size = relocations.len() as u64 * 8;
    let symoff = align_to(
        if relocations.is_empty() {
            text_end
        } else {
            reloc_offset + reloc_size
        },
        8,
    );
    let stroff = symoff + symbols.len() as u64 * 16;

    let mut out = macho_header(SIZEOF_CMDS);
    out.extend_from_slice(&macho_segment_command(
        text_offset as u32,
        text.len() as u64,
        reloc_offset as u32,
        relocations.len() as u32,
    ));
    out.extend_from_slice(&macho_build_version_command());
    out.extend_from_slice(&macho_symtab_command(
        symoff as u32,
        symbols.len() as u32,
        stroff as u32,
        strtab.bytes.len() as u32,
    ));
    out.extend_from_slice(&macho_dysymtab_command(1, external_symbols.len() as u32));

    pad_to(&mut out, text_offset as usize);
    out.extend_from_slice(text);
    if !relocations.is_empty() {
        pad_to(&mut out, reloc_offset as usize);
        for relocation in relocations {
            let symbol_index = symbol_indexes[&relocation.target_abi_symbol];
            out.extend_from_slice(&(relocation.offset as i32).to_le_bytes());
            let info =
                symbol_index | (1 << 24) | (2 << 25) | (1 << 27) | (ARM64_RELOC_BRANCH26 << 28);
            put_u32(&mut out, info);
        }
    }
    pad_to(&mut out, symoff as usize);
    for symbol in &symbols {
        out.extend_from_slice(&symbol.to_bytes());
    }
    pad_to(&mut out, stroff as usize);
    out.extend_from_slice(&strtab.bytes);
    out
}

fn macho_header(sizeofcmds: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    put_u32(&mut out, 0xfeedfacf);
    put_u32(&mut out, 0x0100000c);
    put_u32(&mut out, 0);
    put_u32(&mut out, 1);
    put_u32(&mut out, 4);
    put_u32(&mut out, sizeofcmds);
    put_u32(&mut out, 0x2000);
    put_u32(&mut out, 0);
    debug_assert_eq!(out.len(), 32);
    out
}

fn macho_segment_command(
    text_offset: u32,
    text_size: u64,
    reloc_offset: u32,
    reloc_count: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(152);
    put_u32(&mut out, 0x19);
    put_u32(&mut out, 152);
    put_fixed_name(&mut out, "");
    put_u64(&mut out, 0);
    put_u64(&mut out, text_size);
    put_u64(&mut out, u64::from(text_offset));
    put_u64(&mut out, text_size);
    put_u32(&mut out, 7);
    put_u32(&mut out, 7);
    put_u32(&mut out, 1);
    put_u32(&mut out, 0);

    put_fixed_name(&mut out, "__text");
    put_fixed_name(&mut out, "__TEXT");
    put_u64(&mut out, 0);
    put_u64(&mut out, text_size);
    put_u32(&mut out, text_offset);
    put_u32(&mut out, 2);
    put_u32(&mut out, reloc_offset);
    put_u32(&mut out, reloc_count);
    put_u32(&mut out, 0x80000400);
    put_u32(&mut out, 0);
    put_u32(&mut out, 0);
    put_u32(&mut out, 0);
    debug_assert_eq!(out.len(), 152);
    out
}

fn macho_build_version_command() -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    put_u32(&mut out, 0x32);
    put_u32(&mut out, 24);
    put_u32(&mut out, 1);
    put_u32(&mut out, 11 << 16);
    put_u32(&mut out, 11 << 16);
    put_u32(&mut out, 0);
    out
}

fn macho_symtab_command(symoff: u32, nsyms: u32, stroff: u32, strsize: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    put_u32(&mut out, 0x2);
    put_u32(&mut out, 24);
    put_u32(&mut out, symoff);
    put_u32(&mut out, nsyms);
    put_u32(&mut out, stroff);
    put_u32(&mut out, strsize);
    out
}

fn macho_dysymtab_command(iundefsym: u32, nundefsym: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(80);
    put_u32(&mut out, 0xb);
    put_u32(&mut out, 80);
    for value in [
        0, 0, 0, 1, iundefsym, nundefsym, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ] {
        put_u32(&mut out, value);
    }
    debug_assert_eq!(out.len(), 80);
    out
}

fn put_fixed_name(out: &mut Vec<u8>, name: &str) {
    let mut bytes = [0_u8; 16];
    let name_bytes = name.as_bytes();
    let len = name_bytes.len().min(16);
    bytes[..len].copy_from_slice(&name_bytes[..len]);
    out.extend_from_slice(&bytes);
}

#[derive(Debug, Clone)]
struct MachOSymbolEntry {
    name: u32,
    ty: u8,
    sect: u8,
    desc: u16,
    value: u64,
}

impl MachOSymbolEntry {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        put_u32(&mut out, self.name);
        out.push(self.ty);
        out.push(self.sect);
        put_u16(&mut out, self.desc);
        put_u64(&mut out, self.value);
        out
    }
}

fn macho_symbol_name(abi_symbol: &str) -> String {
    format!("_{abi_symbol}")
}

fn strip_macho_symbol_prefix(symbol: &str) -> &str {
    symbol.strip_prefix('_').unwrap_or(symbol)
}

#[derive(Default)]
struct StringTable {
    bytes: Vec<u8>,
    offsets: BTreeMap<String, u32>,
}

impl StringTable {
    fn insert(&mut self, value: &str) -> u32 {
        if self.bytes.is_empty() {
            self.bytes.push(0);
        }
        if let Some(offset) = self.offsets.get(value) {
            return *offset;
        }
        let offset = self.bytes.len() as u32;
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        self.offsets.insert(value.to_string(), offset);
        offset
    }

    fn offset(&self, value: &str) -> u32 {
        self.offsets[value]
    }
}

fn section_name_table() -> StringTable {
    let mut table = StringTable::default();
    for name in [".text", ".rela.text", ".symtab", ".strtab", ".shstrtab"] {
        table.insert(name);
    }
    table
}

#[derive(Debug, Clone)]
struct SymbolEntry {
    name: u32,
    info: u8,
    shndx: u16,
    value: u64,
    size: u64,
}

impl SymbolEntry {
    fn null() -> Self {
        Self {
            name: 0,
            info: 0,
            shndx: 0,
            value: 0,
            size: 0,
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        put_u32(&mut out, self.name);
        out.push(self.info);
        out.push(0);
        put_u16(&mut out, self.shndx);
        put_u64(&mut out, self.value);
        put_u64(&mut out, self.size);
        out
    }
}

fn elf_header(section_header_offset: u64, section_count: u16, shstrndx: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    put_u16(&mut out, 1);
    put_u16(&mut out, 62);
    put_u32(&mut out, 1);
    put_u64(&mut out, 0);
    put_u64(&mut out, 0);
    put_u64(&mut out, section_header_offset);
    put_u32(&mut out, 0);
    put_u16(&mut out, 64);
    put_u16(&mut out, 0);
    put_u16(&mut out, 0);
    put_u16(&mut out, 64);
    put_u16(&mut out, section_count);
    put_u16(&mut out, shstrndx);
    debug_assert_eq!(out.len(), 64);
    out
}

#[allow(clippy::too_many_arguments)]
fn section_header(
    name: u32,
    section_type: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
) -> [u8; 64] {
    let mut out = Vec::with_capacity(64);
    put_u32(&mut out, name);
    put_u32(&mut out, section_type);
    put_u64(&mut out, flags);
    put_u64(&mut out, 0);
    put_u64(&mut out, offset);
    put_u64(&mut out, size);
    put_u32(&mut out, link);
    put_u32(&mut out, info);
    put_u64(&mut out, align);
    put_u64(&mut out, entsize);
    out.try_into().expect("section header is 64 bytes")
}

fn rela_entry(offset: u64, info: u64, addend: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    put_u64(&mut out, offset);
    put_u64(&mut out, info);
    out.extend_from_slice(&addend.to_le_bytes());
    out
}

fn align_to(value: u64, align: u64) -> u64 {
    if align <= 1 {
        value
    } else {
        value.div_ceil(align) * align
    }
}

fn pad_to(out: &mut Vec<u8>, len: usize) {
    if out.len() < len {
        out.resize(len, 0);
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
