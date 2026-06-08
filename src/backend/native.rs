//! Native object backends for the v0 lowered IR targets.

use std::collections::{BTreeMap, BTreeSet};
use std::thread;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value as JsonValue, json};

use crate::abi::internal_abi_symbol;
use crate::artifact::CacheKeyInput;
use crate::backend::{ArtifactKind, ObjectBackend, ObjectBackendArtifact, ObjectBackendInput};
use crate::jobs::{ArtifactJobClaim, artifact_job_error, new_worker_id};
use crate::lowering::{
    LoweredBlock, LoweredCaseArm, LoweredDebugOp, LoweredFunctionIr, LoweredLocalSlot, LoweredOp,
    LoweredParamSlot, LoweredPlace, LoweredTypeLayout, lowered_op_value_id,
    lowered_value_debug_ops,
};
use crate::model::ProgramRootPayload;
use crate::store::{
    CodeDb, cache_key_for_input, canonical_json, function_interface_metadata, hash_bytes,
};
use crate::types::type_hash_for;
use crate::{APPLE_ARM64_TARGET, BYTES_DOMAIN, LINUX_X86_64_TARGET, MAIN_BRANCH};

pub(crate) const ELF_BACKEND_ID: &str = "native-elf-x86_64-v0";
pub(crate) const MACHO_BACKEND_ID: &str = "native-macho-arm64-v0";
const OBJECT_METADATA_SCHEMA: &str = "codedb/native-object/v1";
const NATIVE_DEBUG_METADATA_SCHEMA: &str = "codedb/native-debug-metadata/v1";
const ELF_OBJECT_FORMAT: &str = "elf64-x86-64-relocatable";
const MACHO_OBJECT_FORMAT: &str = "macho64-arm64-relocatable";
const R_X86_64_PLT32: u32 = 4;
const ARM64_RELOC_BRANCH26: u32 = 2;
const PLATFORM_MALLOC_SYMBOL_HASH: &str = "platform:malloc";
const PLATFORM_FREE_SYMBOL_HASH: &str = "platform:free";
const PLATFORM_MALLOC_ABI_SYMBOL: &str = "malloc";
const PLATFORM_FREE_ABI_SYMBOL: &str = "free";

pub(crate) struct ElfObjectBackend;
pub(crate) struct MachOArm64ObjectBackend;

#[derive(Debug, Clone)]
pub(crate) struct NativeObjectArtifact {
    pub(crate) artifact_hash: String,
    pub(crate) cache_key: String,
    pub(crate) metadata: JsonValue,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct NativeObjectJobInput {
    key_input: CacheKeyInput,
    cache_key: String,
    ir: LoweredFunctionIr,
    dependency_interface_hashes: Vec<String>,
    dependency_implementation_hashes: Vec<String>,
    dependency_closure: Vec<String>,
    target_triple: String,
}

struct ClaimedNativeObjectJob {
    index: usize,
    input: NativeObjectJobInput,
    worker_id: String,
}

struct WaitingNativeObjectJob {
    index: usize,
    input: NativeObjectJobInput,
}

struct NativeObjectCompileOutput {
    artifact_hash: String,
    metadata: JsonValue,
    bytes: Vec<u8>,
}

impl ObjectBackend for ElfObjectBackend {
    fn backend_id(&self) -> &'static str {
        ELF_BACKEND_ID
    }

    fn emit_object(&self, input: ObjectBackendInput<'_>) -> Result<ObjectBackendArtifact> {
        if input.target_triple != LINUX_X86_64_TARGET {
            bail!("{ELF_BACKEND_ID} only supports target {LINUX_X86_64_TARGET}");
        }

        validate_native_ir(input.ir, 6)?;
        let function_symbol = internal_abi_symbol(&input.ir.symbol_hash)?;
        let compiled = compile_x86_64_function(input.ir, &function_symbol)?;
        let bytes = write_elf_object(&function_symbol, &compiled.text, &compiled.relocations);
        let artifact_hash = hash_bytes(BYTES_DOMAIN, &bytes);
        let relocations = compiled
            .relocations
            .iter()
            .map(|relocation| {
                let mut value = json!({
                    "offset": relocation.offset,
                    "kind": "R_X86_64_PLT32",
                    "target_symbol_hash": &relocation.target_symbol_hash,
                    "target_abi_symbol": &relocation.target_abi_symbol,
                });
                if relocation.platform {
                    value
                        .as_object_mut()
                        .unwrap()
                        .insert("platform".to_string(), json!(true));
                }
                value
            })
            .collect::<Vec<_>>();
        let called_symbols = compiled
            .relocations
            .iter()
            .filter(|relocation| !relocation.platform)
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
            "debug_metadata": native_debug_metadata(&compiled),
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

        validate_native_ir(input.ir, 8)?;
        let function_symbol = internal_abi_symbol(&input.ir.symbol_hash)?;
        let object_symbol = macho_symbol_name(&function_symbol);
        let compiled = compile_arm64_function(input.ir, &object_symbol)?;
        let bytes = write_macho_object(&object_symbol, &compiled.text, &compiled.relocations);
        let artifact_hash = hash_bytes(BYTES_DOMAIN, &bytes);
        let relocations = compiled
            .relocations
            .iter()
            .map(|relocation| {
                let mut value = json!({
                    "offset": relocation.offset,
                    "kind": "ARM64_RELOC_BRANCH26",
                    "target_symbol_hash": &relocation.target_symbol_hash,
                    "target_abi_symbol": strip_macho_symbol_prefix(&relocation.target_abi_symbol),
                    "target_object_symbol": &relocation.target_abi_symbol,
                });
                if relocation.platform {
                    value
                        .as_object_mut()
                        .unwrap()
                        .insert("platform".to_string(), json!(true));
                }
                value
            })
            .collect::<Vec<_>>();
        let called_symbols = compiled
            .relocations
            .iter()
            .filter(|relocation| !relocation.platform)
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
            "debug_metadata": native_debug_metadata(&compiled),
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
            .resolve_symbol_or_name(&branch.root_hash, function_name)
            .map_err(|err| anyhow!("unknown entry function {function_name}: {err}"))?;
        self.emit_object_for_symbol(&branch.root_hash, &symbol, target_triple)
    }

    pub(crate) fn emit_object_for_symbol(
        &mut self,
        root_hash: &str,
        symbol: &str,
        target_triple: &str,
    ) -> Result<NativeObjectArtifact> {
        let input = self.native_object_job_input(root_hash, symbol, target_triple)?;
        self.emit_prepared_native_object(input)
    }

    pub(crate) fn emit_objects_for_symbols_parallel(
        &mut self,
        root_hash: &str,
        symbols: &[String],
        target_triple: &str,
    ) -> Result<Vec<NativeObjectArtifact>> {
        let mut results = (0..symbols.len()).map(|_| None).collect::<Vec<_>>();
        let mut handles = Vec::new();
        let mut waiting = Vec::new();

        for (index, symbol) in symbols.iter().enumerate() {
            let input = self.native_object_job_input(root_hash, symbol, target_triple)?;
            if let Some(cache_entry) = self.lookup_cache(&input.key_input)? {
                results[index] = Some(self.native_object_from_cache_entry(&input, cache_entry)?);
                continue;
            }
            let worker_id = new_worker_id("object");
            match self.claim_artifact_job(&input.cache_key, ArtifactKind::ObjectFile, &worker_id)? {
                ArtifactJobClaim::Claimed => {
                    let job = ClaimedNativeObjectJob {
                        index,
                        input,
                        worker_id,
                    };
                    handles.push(thread::spawn(move || {
                        let output = compile_native_object_job(&job.input);
                        (job, output)
                    }));
                }
                ArtifactJobClaim::Succeeded
                | ArtifactJobClaim::Busy {
                    status: _,
                    worker_id: _,
                } => waiting.push(WaitingNativeObjectJob { index, input }),
            }
        }

        let mut first_error = None;
        for handle in handles {
            let (job, output) = handle
                .join()
                .map_err(|_| anyhow!("native object worker thread panicked"))?;
            match output {
                Ok(output) => {
                    let artifact = self.finish_claimed_native_object(&job, output);
                    match artifact {
                        Ok(artifact) => results[job.index] = Some(artifact),
                        Err(err) => {
                            let _ = self.fail_artifact_job(
                                &job.input.cache_key,
                                &job.worker_id,
                                &artifact_job_error("cache_write_failed", format!("{err:#}")),
                            );
                            if first_error.is_none() {
                                first_error = Some(err);
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = self.fail_artifact_job(
                        &job.input.cache_key,
                        &job.worker_id,
                        &artifact_job_error("compile_failed", format!("{err:#}")),
                    );
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
        if let Some(err) = first_error {
            return Err(err);
        }

        for job in waiting {
            let cache_entry =
                self.wait_for_artifact_cache(&job.input.key_input, &job.input.cache_key)?;
            results[job.index] =
                Some(self.native_object_from_cache_entry(&job.input, cache_entry)?);
        }

        results
            .into_iter()
            .map(|artifact| artifact.ok_or_else(|| anyhow!("missing native object result")))
            .collect()
    }

    fn native_object_job_input(
        &mut self,
        root_hash: &str,
        symbol: &str,
        target_triple: &str,
    ) -> Result<NativeObjectJobInput> {
        let root = self.load_root(root_hash)?;
        let lowered = self.lower_symbol_for_target(root_hash, symbol, target_triple)?;
        let dependency_interface_hashes = self.dependency_interface_hashes(&root, &lowered.ir)?;
        let dependency_implementation_hashes =
            self.native_object_type_dependency_hashes(&root, &lowered.ir, target_triple)?;
        let dependency_closure = self.dependency_closure_for_symbol(root_hash, symbol)?;
        let backend_id = backend_id_for_target(target_triple)?;
        let key_input = CacheKeyInput::new(
            ArtifactKind::ObjectFile,
            &lowered.ir.function_def_hash,
            backend_id,
            target_triple,
        )
        .with_dependency_interface_hashes(dependency_interface_hashes.clone())
        .with_dependency_implementation_hashes(dependency_implementation_hashes.clone());
        let object_cache_key = cache_key_for_input(&key_input)?;

        Ok(NativeObjectJobInput {
            key_input,
            cache_key: object_cache_key,
            ir: lowered.ir,
            dependency_interface_hashes,
            dependency_implementation_hashes,
            dependency_closure,
            target_triple: target_triple.to_string(),
        })
    }

    fn emit_prepared_native_object(
        &mut self,
        input: NativeObjectJobInput,
    ) -> Result<NativeObjectArtifact> {
        if let Some(cache_entry) = self.lookup_cache(&input.key_input)? {
            return self.native_object_from_cache_entry(&input, cache_entry);
        }
        let worker_id = new_worker_id("object");
        match self.claim_artifact_job(&input.cache_key, ArtifactKind::ObjectFile, &worker_id)? {
            ArtifactJobClaim::Claimed => {
                let output = match compile_native_object_job(&input) {
                    Ok(output) => output,
                    Err(err) => {
                        let _ = self.fail_artifact_job(
                            &input.cache_key,
                            &worker_id,
                            &artifact_job_error("compile_failed", format!("{err:#}")),
                        );
                        return Err(err);
                    }
                };
                let job = ClaimedNativeObjectJob {
                    index: 0,
                    input,
                    worker_id,
                };
                self.finish_claimed_native_object(&job, output)
            }
            ArtifactJobClaim::Succeeded
            | ArtifactJobClaim::Busy {
                status: _,
                worker_id: _,
            } => {
                let cache_entry =
                    self.wait_for_artifact_cache(&input.key_input, &input.cache_key)?;
                self.native_object_from_cache_entry(&input, cache_entry)
            }
        }
    }

    fn finish_claimed_native_object(
        &mut self,
        job: &ClaimedNativeObjectJob,
        output: NativeObjectCompileOutput,
    ) -> Result<NativeObjectArtifact> {
        self.write_cache_bytes(job.input.key_input.clone(), &output.metadata, &output.bytes)?;
        self.complete_artifact_job(&job.input.cache_key, &job.worker_id)?;
        Ok(NativeObjectArtifact {
            artifact_hash: output.artifact_hash,
            cache_key: job.input.cache_key.clone(),
            metadata: output.metadata,
            bytes: output.bytes,
        })
    }

    fn native_object_from_cache_entry(
        &mut self,
        input: &NativeObjectJobInput,
        cache_entry: crate::store::CacheEntry,
    ) -> Result<NativeObjectArtifact> {
        let bytes = cache_entry
            .artifact_bytes
            .ok_or_else(|| anyhow!("object cache entry missing artifact_bytes"))?;
        let artifact_json = cache_entry
            .artifact_json
            .ok_or_else(|| anyhow!("object cache entry missing artifact_json"))?;
        let mut metadata = object_metadata_from_cache(&artifact_json)?;
        let original_metadata = metadata.clone();
        add_native_object_dependency_metadata(
            &mut metadata,
            &input.dependency_interface_hashes,
            &input.dependency_implementation_hashes,
            &input.dependency_closure,
        )?;
        if metadata != original_metadata {
            self.write_cache_bytes(input.key_input.clone(), &metadata, &bytes)?;
        }
        Ok(NativeObjectArtifact {
            artifact_hash: cache_entry.artifact_hash,
            cache_key: input.cache_key.clone(),
            metadata,
            bytes,
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

    pub(crate) fn native_object_type_dependency_hashes(
        &self,
        root: &ProgramRootPayload,
        ir: &LoweredFunctionIr,
        target_triple: &str,
    ) -> Result<Vec<String>> {
        let mut dependencies = BTreeSet::new();
        for layout in &ir.type_layouts {
            dependencies.extend(
                self.compute_type_layout(root, &layout.type_hash, target_triple)?
                    .dependency_type_def_hashes,
            );
        }
        Ok(dependencies.into_iter().collect())
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
    dependency_implementation_hashes: &[String],
    dependency_closure: &[String],
) -> Result<()> {
    let object = metadata
        .as_object_mut()
        .ok_or_else(|| anyhow!("native object metadata must be a JSON object"))?;
    object.insert(
        "dependency_interface_hashes".to_string(),
        json!(dependency_interface_hashes),
    );
    object.insert(
        "dependency_implementation_hashes".to_string(),
        json!(dependency_implementation_hashes),
    );
    object.insert("dependency_closure".to_string(), json!(dependency_closure));
    Ok(())
}

fn compile_native_object_job(input: &NativeObjectJobInput) -> Result<NativeObjectCompileOutput> {
    let backend_input = ObjectBackendInput {
        ir: &input.ir,
        target_triple: &input.target_triple,
    };
    let emitted = match input.target_triple.as_str() {
        LINUX_X86_64_TARGET => ElfObjectBackend.emit_object(backend_input)?,
        APPLE_ARM64_TARGET => MachOArm64ObjectBackend.emit_object(backend_input)?,
        _ => unreachable!("unsupported target was checked by backend_id_for_target"),
    };
    let mut metadata = emitted.metadata;
    add_native_object_dependency_metadata(
        &mut metadata,
        &input.dependency_interface_hashes,
        &input.dependency_implementation_hashes,
        &input.dependency_closure,
    )?;
    Ok(NativeObjectCompileOutput {
        artifact_hash: emitted.artifact_hash,
        metadata,
        bytes: emitted.bytes,
    })
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

pub(crate) fn backend_id_for_target(target_triple: &str) -> Result<&'static str> {
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
            LoweredOp::Case { arms, .. } => {
                for arm in arms {
                    collect_called_symbols(&arm.block.operations, out);
                }
            }
            LoweredOp::Fold { body, .. } => {
                collect_called_symbols(&body.operations, out);
            }
            LoweredOp::Param { .. }
            | LoweredOp::ConstI64 { .. }
            | LoweredOp::ConstBool { .. }
            | LoweredOp::ConstUnit { .. }
            | LoweredOp::Unary { .. }
            | LoweredOp::Binary { .. }
            | LoweredOp::BorrowShared { .. }
            | LoweredOp::BorrowMut { .. }
            | LoweredOp::DerefShared { .. }
            | LoweredOp::DerefMut { .. }
            | LoweredOp::DerefBox { .. }
            | LoweredOp::HeapAlloc { .. }
            | LoweredOp::AddrOfParam { .. }
            | LoweredOp::AddrOfLocal { .. }
            | LoweredOp::AddrOfField { .. }
            | LoweredOp::AddrOfEnumPayload { .. }
            | LoweredOp::AddrOfIndex { .. }
            | LoweredOp::ConstructSlice { .. }
            | LoweredOp::SliceLen { .. }
            | LoweredOp::SliceData { .. }
            | LoweredOp::BoundsCheck { .. }
            | LoweredOp::SliceRangeCheck { .. }
            | LoweredOp::LoadEnumTag { .. }
            | LoweredOp::Load { .. }
            | LoweredOp::StoreEnumTag { .. }
            | LoweredOp::Store { .. }
            | LoweredOp::Copy { .. }
            | LoweredOp::Move { .. }
            | LoweredOp::Drop { .. }
            | LoweredOp::BorrowDebug { .. }
            | LoweredOp::Return { .. } => {}
        }
    }
}

fn validate_native_ir(ir: &LoweredFunctionIr, max_machine_params: usize) -> Result<()> {
    let i64_type = type_hash_for("I64");
    let bool_type = type_hash_for("Bool");
    let unit_type = type_hash_for("Unit");
    let type_layouts = native_type_layouts(ir)?;
    native_supported_type(
        &type_layouts,
        &ir.return_type_hash,
        &i64_type,
        &bool_type,
        &unit_type,
    )?;
    let hidden_return_count = usize::from(native_returns_indirect(
        &type_layouts,
        &ir.return_type_hash,
    )?);
    if ir.params.len() + hidden_return_count > max_machine_params {
        bail!("native object backend v0 supports at most {max_machine_params} machine parameters");
    }
    for param in &ir.params {
        native_supported_type(
            &type_layouts,
            &param.type_hash,
            &i64_type,
            &bool_type,
            &unit_type,
        )?;
    }
    for local in &ir.locals {
        if local.size_bytes == 0 || !local.size_bytes.is_multiple_of(8) {
            bail!("native object backend v0 local slots must be nonzero multiples of 8 bytes");
        }
    }
    let mut values = BTreeMap::new();
    let mut addresses = BTreeMap::new();
    validate_native_ops(
        &ir.operations,
        &ir.params,
        &ir.locals,
        &i64_type,
        &bool_type,
        &unit_type,
        &type_layouts,
        &mut values,
        &mut addresses,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_native_ops(
    operations: &[LoweredOp],
    params: &[LoweredParamSlot],
    locals: &[LoweredLocalSlot],
    i64_type: &str,
    bool_type: &str,
    unit_type: &str,
    type_layouts: &BTreeMap<String, LoweredTypeLayout>,
    values: &mut BTreeMap<String, String>,
    addresses: &mut BTreeMap<String, String>,
) -> Result<()> {
    for op in operations {
        match op {
            LoweredOp::Param { type_hash, .. }
            | LoweredOp::ConstI64 { type_hash, .. }
            | LoweredOp::ConstBool { type_hash, .. }
            | LoweredOp::ConstUnit { type_hash, .. }
            | LoweredOp::Unary { type_hash, .. }
            | LoweredOp::Binary { type_hash, .. }
            | LoweredOp::Call { type_hash, .. }
            | LoweredOp::If { type_hash, .. }
            | LoweredOp::Case { type_hash, .. }
            | LoweredOp::Fold { type_hash, .. }
            | LoweredOp::HeapAlloc { type_hash, .. }
            | LoweredOp::Return { type_hash, .. } => {
                native_supported_type(type_layouts, type_hash, i64_type, bool_type, unit_type)?;
            }
            LoweredOp::BorrowShared { .. }
            | LoweredOp::BorrowMut { .. }
            | LoweredOp::DerefShared { .. }
            | LoweredOp::DerefMut { .. }
            | LoweredOp::DerefBox { .. }
            | LoweredOp::ConstructSlice { .. }
            | LoweredOp::SliceLen { .. }
            | LoweredOp::SliceData { .. } => {}
            LoweredOp::AddrOfParam { place, .. } => {
                let LoweredPlace::Param {
                    slot, type_hash, ..
                } = place
                else {
                    bail!("addr_of_param must contain a param place");
                };
                if params
                    .get(*slot)
                    .is_none_or(|param| param.slot != *slot || param.type_hash != *type_hash)
                {
                    bail!("native object backend saw invalid addr_of_param");
                }
            }
            LoweredOp::AddrOfLocal { place, .. } => {
                let LoweredPlace::Local { slot, type_hash } = place else {
                    bail!("addr_of_local must contain a local place");
                };
                if locals
                    .get(*slot)
                    .is_none_or(|local| local.slot != *slot || local.type_hash != *type_hash)
                {
                    bail!("native object backend saw invalid addr_of_local");
                }
            }
            LoweredOp::AddrOfField { .. } => {}
            LoweredOp::AddrOfEnumPayload { .. } => {}
            LoweredOp::AddrOfIndex { .. } => {}
            LoweredOp::BoundsCheck { type_hash, .. } => {
                if type_hash != unit_type {
                    bail!("native object backend saw bounds_check with non-unit type");
                }
            }
            LoweredOp::SliceRangeCheck { type_hash, .. } => {
                if type_hash != unit_type {
                    bail!("native object backend saw slice_range_check with non-unit type");
                }
            }
            LoweredOp::LoadEnumTag { type_hash, .. } => {
                let layout = native_type_layout(type_layouts, type_hash)?;
                if layout.kind != "enum" {
                    bail!("native object backend saw load_enum_tag for non-enum type");
                }
            }
            LoweredOp::Load { type_hash, .. }
            | LoweredOp::Store { type_hash, .. }
            | LoweredOp::StoreEnumTag { type_hash, .. }
            | LoweredOp::Copy { type_hash, .. }
            | LoweredOp::Move { type_hash, .. }
            | LoweredOp::Drop { type_hash, .. }
            | LoweredOp::BorrowDebug { type_hash, .. } => {
                let _ = type_hash;
            }
        }
        validate_native_op_flow(op, params, locals, type_layouts, values, addresses)?;
        if let LoweredOp::If {
            then_block,
            else_block,
            ..
        } = op
        {
            let mut then_values = values.clone();
            let mut then_addresses = addresses.clone();
            validate_native_ops(
                &then_block.operations,
                params,
                locals,
                i64_type,
                bool_type,
                unit_type,
                type_layouts,
                &mut then_values,
                &mut then_addresses,
            )?;
            let mut else_values = values.clone();
            let mut else_addresses = addresses.clone();
            validate_native_ops(
                &else_block.operations,
                params,
                locals,
                i64_type,
                bool_type,
                unit_type,
                type_layouts,
                &mut else_values,
                &mut else_addresses,
            )?;
        } else if let LoweredOp::Case { arms, .. } = op {
            for arm in arms {
                let mut arm_values = values.clone();
                let mut arm_addresses = addresses.clone();
                validate_native_ops(
                    &arm.block.operations,
                    params,
                    locals,
                    i64_type,
                    bool_type,
                    unit_type,
                    type_layouts,
                    &mut arm_values,
                    &mut arm_addresses,
                )?;
            }
        } else if let LoweredOp::Fold {
            body,
            acc_type_hash,
            ..
        } = op
        {
            let mut body_values = values.clone();
            let mut body_addresses = addresses.clone();
            validate_native_ops(
                &body.operations,
                params,
                locals,
                i64_type,
                bool_type,
                unit_type,
                type_layouts,
                &mut body_values,
                &mut body_addresses,
            )?;
            if native_value_type(&body_values, &body.result)? != acc_type_hash {
                bail!("native object backend saw fold body result type mismatch");
            }
        }
    }
    Ok(())
}

fn native_supported_type(
    type_layouts: &BTreeMap<String, LoweredTypeLayout>,
    type_hash: &str,
    i64_type: &str,
    bool_type: &str,
    unit_type: &str,
) -> Result<()> {
    if type_hash == i64_type || type_hash == bool_type || type_hash == unit_type {
        return Ok(());
    }
    let layout = native_type_layout(type_layouts, type_hash)?;
    match layout.kind.as_str() {
        "record" | "enum" | "fixed_array" | "slice" | "reference" | "raw_pointer" | "box" => Ok(()),
        other => bail!("native object backend v0 does not support native values of {other} type"),
    }
}

fn validate_native_op_flow(
    op: &LoweredOp,
    params: &[LoweredParamSlot],
    locals: &[LoweredLocalSlot],
    type_layouts: &BTreeMap<String, LoweredTypeLayout>,
    values: &mut BTreeMap<String, String>,
    addresses: &mut BTreeMap<String, String>,
) -> Result<()> {
    match op {
        LoweredOp::Param {
            id,
            slot,
            type_hash,
        } => {
            if params
                .get(*slot)
                .is_none_or(|param| param.slot != *slot || param.type_hash != *type_hash)
            {
                bail!("native object backend saw invalid param op");
            }
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::ConstI64 { id, type_hash, .. }
        | LoweredOp::ConstBool { id, type_hash, .. }
        | LoweredOp::ConstUnit { id, type_hash } => {
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::Unary {
            id,
            value,
            type_hash,
            ..
        } => {
            native_value_type(values, value)?;
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::Binary {
            id,
            left,
            right,
            type_hash,
            ..
        } => {
            native_value_type(values, left)?;
            native_value_type(values, right)?;
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::Call {
            id,
            args,
            return_address,
            type_hash,
            ..
        } => {
            for arg in args {
                native_value_type(values, arg)?;
            }
            if native_returns_indirect(type_layouts, type_hash)? {
                let Some(return_address) = return_address else {
                    bail!("native object backend saw aggregate call without return address");
                };
                if native_address_type(addresses, return_address)? != type_hash {
                    bail!("native object backend saw aggregate call return address mismatch");
                }
                native_insert_address(addresses, id, type_hash)?;
            } else if return_address.is_some() {
                bail!("native object backend saw scalar call with return address");
            }
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::If {
            id,
            cond,
            type_hash,
            ..
        } => {
            native_value_type(values, cond)?;
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::Case {
            id,
            scrutinee,
            enum_type_hash,
            arms,
            type_hash,
        } => {
            if native_value_type(values, scrutinee)? != enum_type_hash {
                bail!("native object backend saw case scrutinee type mismatch");
            }
            if native_passes_indirect(type_layouts, enum_type_hash)?
                && native_address_type(addresses, scrutinee)? != enum_type_hash
            {
                bail!("native object backend saw case scrutinee address mismatch");
            }
            let enum_layout = native_type_layout(type_layouts, enum_type_hash)?;
            if enum_layout.kind != "enum" {
                bail!("native object backend saw case over non-enum type");
            }
            if arms.is_empty() {
                bail!("native object backend saw case without arms");
            }
            let mut seen_tags = BTreeSet::new();
            for arm in arms {
                native_case_arm_layout(type_layouts, enum_type_hash, arm)?;
                if !seen_tags.insert(arm.tag_value) {
                    bail!(
                        "native object backend saw duplicate case tag {}",
                        arm.tag_value
                    );
                }
            }
            native_insert_value(values, id, type_hash)?;
            if native_passes_indirect(type_layouts, type_hash)? {
                native_insert_address(addresses, id, type_hash)?;
            }
        }
        LoweredOp::Fold {
            id,
            target_address,
            target_type_hash,
            len,
            init,
            index_slot,
            acc_slot,
            item_slot,
            body,
            element_type_hash,
            acc_type_hash,
            type_hash,
        } => {
            if type_hash != acc_type_hash {
                bail!("native object backend saw fold result/accumulator type mismatch");
            }
            if native_address_type(addresses, target_address)? != target_type_hash {
                bail!("native object backend saw fold target address mismatch");
            }
            if native_value_type(values, len)? != &type_hash_for("I64")
                || native_value_type(values, init)? != acc_type_hash
            {
                bail!("native object backend saw fold value type mismatch");
            }
            if locals
                .get(*index_slot)
                .is_none_or(|local| local.type_hash != type_hash_for("I64"))
                || locals
                    .get(*acc_slot)
                    .is_none_or(|local| local.type_hash != *acc_type_hash)
                || locals
                    .get(*item_slot)
                    .is_none_or(|local| local.type_hash != *element_type_hash)
            {
                bail!("native object backend saw fold local slot type mismatch");
            }
            match native_type_layout(type_layouts, target_type_hash)?
                .kind
                .as_str()
            {
                "fixed_array" | "slice" => {}
                other => bail!("native object backend saw fold over {other}"),
            }
            native_type_layout(type_layouts, element_type_hash)?;
            let _ = body;
            native_insert_value(values, id, type_hash)?;
            if native_passes_indirect(type_layouts, type_hash)? {
                native_insert_address(addresses, id, type_hash)?;
            }
        }
        LoweredOp::BorrowShared {
            id,
            address,
            referent_type_hash,
            type_hash,
            ..
        }
        | LoweredOp::BorrowMut {
            id,
            address,
            referent_type_hash,
            type_hash,
            ..
        } => {
            if native_address_type(addresses, address)? != referent_type_hash {
                bail!("native object backend saw borrow referent mismatch");
            }
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::DerefShared {
            id,
            reference,
            referent_type_hash,
        }
        | LoweredOp::DerefMut {
            id,
            reference,
            referent_type_hash,
        } => {
            native_value_type(values, reference)?;
            native_insert_address(addresses, id, referent_type_hash)?;
            native_insert_value(values, id, referent_type_hash)?;
        }
        LoweredOp::DerefBox {
            id,
            box_value,
            box_type_hash,
            element_type_hash,
        } => {
            if native_value_type(values, box_value)? != box_type_hash {
                bail!("native object backend saw deref_box value type mismatch");
            }
            let box_layout = native_type_layout(type_layouts, box_type_hash)?;
            if box_layout.kind != "box"
                || native_layout_string(box_layout, "element_type_hash")? != *element_type_hash
            {
                bail!("native object backend saw deref_box layout mismatch");
            }
            native_type_layout(type_layouts, element_type_hash)?;
            native_insert_address(addresses, id, element_type_hash)?;
            native_insert_value(values, id, element_type_hash)?;
        }
        LoweredOp::HeapAlloc {
            id,
            size_bytes,
            align_bytes,
            element_type_hash,
            type_hash,
        } => {
            if *size_bytes == 0 || *align_bytes == 0 {
                bail!("native object backend saw heap_alloc with zero layout");
            }
            let box_layout = native_type_layout(type_layouts, type_hash)?;
            if box_layout.kind != "box"
                || native_layout_string(box_layout, "element_type_hash")? != *element_type_hash
            {
                bail!("native object backend saw heap_alloc non-box result");
            }
            let element_layout = native_type_layout(type_layouts, element_type_hash)?;
            if element_layout.size_bytes != *size_bytes
                || element_layout.align_bytes != *align_bytes
            {
                bail!("native object backend saw heap_alloc element layout mismatch");
            }
            native_insert_value(values, id, type_hash)?;
            native_insert_address(addresses, id, element_type_hash)?;
        }
        LoweredOp::AddrOfParam { id, place } => {
            let LoweredPlace::Param {
                slot, type_hash, ..
            } = place
            else {
                bail!("addr_of_param must contain a param place");
            };
            if params
                .get(*slot)
                .is_none_or(|param| param.slot != *slot || param.type_hash != *type_hash)
            {
                bail!("native object backend saw invalid addr_of_param");
            }
            native_insert_address(addresses, id, type_hash)?;
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::AddrOfLocal { id, place } => {
            let LoweredPlace::Local { slot, type_hash } = place else {
                bail!("addr_of_local must contain a local place");
            };
            if locals
                .get(*slot)
                .is_none_or(|local| local.slot != *slot || local.type_hash != *type_hash)
            {
                bail!("native object backend saw invalid addr_of_local");
            }
            native_insert_address(addresses, id, type_hash)?;
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::AddrOfField { id, place } => {
            let LoweredPlace::Field {
                base,
                owner_type_hash,
                type_hash,
                ..
            } = place
            else {
                bail!("addr_of_field must contain a field place");
            };
            if native_address_type(addresses, base)? != owner_type_hash {
                bail!("native object backend saw addr_of_field owner mismatch");
            }
            native_insert_address(addresses, id, type_hash)?;
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::AddrOfEnumPayload { id, place } => {
            let LoweredPlace::EnumPayload {
                base,
                owner_type_hash,
                variant,
                tag_value,
                payload_offset_bytes,
                type_hash,
                ..
            } = place
            else {
                bail!("addr_of_enum_payload must contain an enum payload place");
            };
            if native_address_type(addresses, base)? != owner_type_hash {
                bail!("native object backend saw addr_of_enum_payload owner mismatch");
            }
            let arm = LoweredCaseArm {
                variant: variant.clone(),
                variant_symbol: None,
                tag_value: *tag_value,
                payload_type_hash: type_hash.clone(),
                payload_offset_bytes: *payload_offset_bytes,
                block: LoweredBlock {
                    operations: Vec::new(),
                    result: String::new(),
                },
            };
            native_case_arm_layout(type_layouts, owner_type_hash, &arm)?;
            native_insert_address(addresses, id, type_hash)?;
            native_insert_value(values, id, type_hash)?;
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
            let base_type = native_address_type(addresses, base)?;
            if base_type == element_type_hash {
                // Slice indexing passes the already-loaded element base pointer.
            } else if native_type_layout(type_layouts, base_type)?.kind != "fixed_array" {
                bail!("native object backend saw addr_of_index for non-array base");
            }
            if native_value_type(values, index)? != &type_hash_for("I64") {
                bail!("native object backend saw addr_of_index with non-i64 index");
            }
            if element_type_hash != type_hash {
                bail!("native object backend saw addr_of_index element type mismatch");
            }
            native_type_layout(type_layouts, element_type_hash)?;
            native_insert_address(addresses, id, type_hash)?;
            match native_type_layout(type_layouts, type_hash)?.kind.as_str() {
                "record" | "enum" | "fixed_array" => native_insert_value(values, id, type_hash)?,
                _ => {}
            }
        }
        LoweredOp::ConstructSlice {
            id,
            address,
            data_address,
            len,
            element_type_hash,
            type_hash,
        } => {
            if native_address_type(addresses, address)? != type_hash {
                bail!("native object backend saw construct_slice address mismatch");
            }
            let data_type = native_address_type(addresses, data_address)?;
            if data_type != element_type_hash
                && native_type_layout(type_layouts, data_type)?.kind != "fixed_array"
            {
                bail!("native object backend saw construct_slice data mismatch");
            }
            if native_value_type(values, len)? != &type_hash_for("I64") {
                bail!("native object backend saw construct_slice with non-i64 len");
            }
            if native_type_layout(type_layouts, type_hash)?.kind != "slice" {
                bail!("native object backend saw construct_slice for non-slice type");
            }
            native_type_layout(type_layouts, element_type_hash)?;
            native_insert_value(values, id, type_hash)?;
            native_insert_address(addresses, id, type_hash)?;
        }
        LoweredOp::SliceLen {
            id,
            slice,
            slice_type_hash,
            type_hash,
        } => {
            if native_address_type(addresses, slice)? != slice_type_hash {
                bail!("native object backend saw slice_len address mismatch");
            }
            if native_type_layout(type_layouts, slice_type_hash)?.kind != "slice" {
                bail!("native object backend saw slice_len for non-slice type");
            }
            if type_hash != &type_hash_for("I64") {
                bail!("native object backend saw slice_len non-i64 result");
            }
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::SliceData {
            id,
            slice,
            slice_type_hash,
            element_type_hash,
        } => {
            if native_address_type(addresses, slice)? != slice_type_hash {
                bail!("native object backend saw slice_data address mismatch");
            }
            if native_type_layout(type_layouts, slice_type_hash)?.kind != "slice" {
                bail!("native object backend saw slice_data for non-slice type");
            }
            native_type_layout(type_layouts, element_type_hash)?;
            native_insert_address(addresses, id, element_type_hash)?;
        }
        LoweredOp::BoundsCheck {
            id,
            index,
            len: _,
            len_value,
            type_hash,
        } => {
            if native_value_type(values, index)? != &type_hash_for("I64") {
                bail!("native object backend saw bounds_check with non-i64 index");
            }
            if let Some(len_value) = len_value
                && native_value_type(values, len_value)? != &type_hash_for("I64")
            {
                bail!("native object backend saw bounds_check with non-i64 len_value");
            }
            if type_hash != &type_hash_for("Unit") {
                bail!("native object backend saw bounds_check with non-unit type");
            }
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::SliceRangeCheck {
            id,
            start,
            len,
            source_len,
            type_hash,
        } => {
            if native_value_type(values, start)? != &type_hash_for("I64")
                || native_value_type(values, len)? != &type_hash_for("I64")
                || native_value_type(values, source_len)? != &type_hash_for("I64")
            {
                bail!("native object backend saw slice_range_check with non-i64 value");
            }
            if type_hash != &type_hash_for("Unit") {
                bail!("native object backend saw slice_range_check with non-unit type");
            }
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::LoadEnumTag {
            id,
            address,
            type_hash,
        } => {
            if native_address_type(addresses, address)? != type_hash {
                bail!("native object backend saw load_enum_tag address mismatch");
            }
            if native_type_layout(type_layouts, type_hash)?.kind != "enum" {
                bail!("native object backend saw load_enum_tag for non-enum");
            }
            native_insert_value(values, id, &type_hash_for("I64"))?;
        }
        LoweredOp::Load {
            id,
            address,
            type_hash,
        } => {
            if native_address_type(addresses, address)? != type_hash {
                bail!("native object backend saw load type mismatch");
            }
            native_insert_value(values, id, type_hash)?;
        }
        LoweredOp::Store {
            address,
            value,
            type_hash,
        } => {
            if native_address_type(addresses, address)? != type_hash {
                bail!("native object backend saw store address type mismatch");
            }
            let actual = native_value_type(values, value)?;
            if actual != type_hash && !native_layout_compatible(type_layouts, actual, type_hash)? {
                bail!("native object backend saw store value type mismatch");
            }
        }
        LoweredOp::StoreEnumTag {
            address,
            type_hash,
            variant: _,
            tag_value,
            ..
        } => {
            if native_address_type(addresses, address)? != type_hash {
                bail!("native object backend saw store_enum_tag address mismatch");
            }
            if native_type_layout(type_layouts, type_hash)?.kind != "enum" {
                bail!("native object backend saw store_enum_tag for non-enum");
            }
            if *tag_value > i32::MAX as u64 {
                bail!("native object backend saw unencodable enum tag {tag_value}");
            }
        }
        LoweredOp::Copy {
            id,
            value,
            type_hash,
        } => {
            if native_value_type(values, value)? != type_hash {
                bail!("native object backend saw copy type mismatch");
            }
            native_insert_value(values, id, type_hash)?;
            if native_passes_indirect(type_layouts, type_hash)? {
                native_insert_address(addresses, id, type_hash)?;
            }
        }
        LoweredOp::Move {
            id,
            address,
            type_hash,
        } => {
            if native_address_type(addresses, address)? != type_hash {
                bail!("native object backend saw move type mismatch");
            }
            native_insert_value(values, id, type_hash)?;
            if native_passes_indirect(type_layouts, type_hash)? {
                native_insert_address(addresses, id, type_hash)?;
            }
        }
        LoweredOp::Drop { address, type_hash }
        | LoweredOp::BorrowDebug {
            address, type_hash, ..
        } => {
            if native_address_type(addresses, address)? != type_hash {
                bail!("native object backend saw metadata/drop type mismatch");
            }
        }
        LoweredOp::Return { value, type_hash } => {
            let actual = native_value_type(values, value)?;
            if actual != type_hash && !native_layout_compatible(type_layouts, actual, type_hash)? {
                bail!("native object backend saw return type mismatch");
            }
        }
    }
    Ok(())
}

fn native_insert_value(
    values: &mut BTreeMap<String, String>,
    id: &str,
    type_hash: &str,
) -> Result<()> {
    if values
        .insert(id.to_string(), type_hash.to_string())
        .is_some()
    {
        bail!("duplicate native lowered value id {id}");
    }
    Ok(())
}

fn native_insert_address(
    addresses: &mut BTreeMap<String, String>,
    id: &str,
    type_hash: &str,
) -> Result<()> {
    if addresses
        .insert(id.to_string(), type_hash.to_string())
        .is_some()
    {
        bail!("duplicate native lowered address id {id}");
    }
    Ok(())
}

fn native_value_type<'a>(values: &'a BTreeMap<String, String>, id: &str) -> Result<&'a String> {
    values
        .get(id)
        .ok_or_else(|| anyhow!("unknown native lowered value id {id}"))
}

fn native_address_type<'a>(
    addresses: &'a BTreeMap<String, String>,
    id: &str,
) -> Result<&'a String> {
    addresses
        .get(id)
        .ok_or_else(|| anyhow!("unknown native lowered address id {id}"))
}

fn native_type_layouts(ir: &LoweredFunctionIr) -> Result<BTreeMap<String, LoweredTypeLayout>> {
    let mut layouts = BTreeMap::new();
    for layout in &ir.type_layouts {
        if layouts
            .insert(layout.type_hash.clone(), layout.clone())
            .is_some()
        {
            bail!("duplicate native type layout {}", layout.type_hash);
        }
    }
    Ok(layouts)
}

fn native_type_layout<'a>(
    layouts: &'a BTreeMap<String, LoweredTypeLayout>,
    type_hash: &str,
) -> Result<&'a LoweredTypeLayout> {
    layouts
        .get(type_hash)
        .ok_or_else(|| anyhow!("native object backend missing type layout for {type_hash}"))
}

fn native_case_arm_layout(
    layouts: &BTreeMap<String, LoweredTypeLayout>,
    enum_type_hash: &str,
    arm: &LoweredCaseArm,
) -> Result<()> {
    let layout = native_type_layout(layouts, enum_type_hash)?;
    if layout.kind != "enum" {
        bail!("native object backend expected enum layout for {enum_type_hash}");
    }
    if arm.payload_offset_bytes > layout.size_bytes {
        bail!("native object backend saw enum payload offset outside layout");
    }
    let payload = native_type_layout(layouts, &arm.payload_type_hash)?;
    if arm.payload_offset_bytes + payload.size_bytes > layout.size_bytes {
        bail!("native object backend saw enum payload outside layout bounds");
    }
    Ok(())
}

fn native_type_size(layouts: &BTreeMap<String, LoweredTypeLayout>, type_hash: &str) -> Result<u64> {
    Ok(native_type_layout(layouts, type_hash)?.size_bytes)
}

fn native_array_stride(
    layouts: &BTreeMap<String, LoweredTypeLayout>,
    element_type_hash: &str,
) -> Result<u64> {
    let element = native_type_layout(layouts, element_type_hash)?;
    if element.align_bytes == 0 {
        bail!("native object backend saw zero element alignment");
    }
    Ok(element.size_bytes.div_ceil(element.align_bytes) * element.align_bytes)
}

fn native_stack_slot_size_bytes(layout_size_bytes: u64) -> u64 {
    layout_size_bytes.max(1).div_ceil(8) * 8
}

fn native_passes_indirect(
    layouts: &BTreeMap<String, LoweredTypeLayout>,
    type_hash: &str,
) -> Result<bool> {
    Ok(native_type_layout(layouts, type_hash)?.abi.pass == "by_indirect")
}

fn native_returns_indirect(
    layouts: &BTreeMap<String, LoweredTypeLayout>,
    type_hash: &str,
) -> Result<bool> {
    Ok(native_type_layout(layouts, type_hash)?.abi.return_ == "hidden_return_slot")
}

fn native_layout_compatible(
    layouts: &BTreeMap<String, LoweredTypeLayout>,
    actual: &str,
    expected: &str,
) -> Result<bool> {
    if actual == expected {
        return Ok(true);
    }
    let actual = native_type_layout(layouts, actual)?;
    let expected = native_type_layout(layouts, expected)?;
    Ok(actual.kind == expected.kind
        && actual.size_bytes == expected.size_bytes
        && actual.align_bytes == expected.align_bytes
        && actual.abi == expected.abi)
}

fn native_layout_string(layout: &LoweredTypeLayout, key: &str) -> Result<String> {
    layout
        .metadata
        .get(key)
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "native type layout {} missing string {key}",
                layout.type_hash
            )
        })
}

fn native_layout_u64(layout: &LoweredTypeLayout, key: &str) -> Result<u64> {
    layout
        .metadata
        .get(key)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| {
            anyhow!(
                "native type layout {} missing integer {key}",
                layout.type_hash
            )
        })
}

fn native_contains_box(
    layouts: &BTreeMap<String, LoweredTypeLayout>,
    type_hash: &str,
) -> Result<bool> {
    Ok(native_type_layout(layouts, type_hash)?
        .metadata
        .get("contains_box")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false))
}

fn native_needs_drop(
    layouts: &BTreeMap<String, LoweredTypeLayout>,
    type_hash: &str,
) -> Result<bool> {
    let layout = native_type_layout(layouts, type_hash)?;
    Ok(layout.metadata.get("drop_kind").and_then(JsonValue::as_str) == Some("needs_drop"))
}

#[derive(Debug, Clone)]
struct NativeFieldLayout {
    type_hash: String,
    offset_bytes: u64,
}

#[derive(Debug, Clone)]
struct NativeVariantLayout {
    type_hash: String,
    tag_value: u64,
    payload_offset_bytes: u64,
}

fn native_record_fields(layout: &LoweredTypeLayout) -> Result<Vec<NativeFieldLayout>> {
    if layout.kind != "record" {
        bail!("native type layout {} is not a record", layout.type_hash);
    }
    layout
        .metadata
        .get("fields")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| anyhow!("native record layout {} missing fields", layout.type_hash))?
        .iter()
        .map(|field| {
            Ok(NativeFieldLayout {
                type_hash: field
                    .get("type_hash")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("native record layout field missing type_hash"))?
                    .to_string(),
                offset_bytes: field
                    .get("offset_bytes")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("native record layout field missing offset_bytes"))?,
            })
        })
        .collect()
}

fn native_enum_variants(layout: &LoweredTypeLayout) -> Result<Vec<NativeVariantLayout>> {
    if layout.kind != "enum" {
        bail!("native type layout {} is not an enum", layout.type_hash);
    }
    layout
        .metadata
        .get("variants")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| anyhow!("native enum layout {} missing variants", layout.type_hash))?
        .iter()
        .map(|variant| {
            Ok(NativeVariantLayout {
                type_hash: variant
                    .get("type_hash")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("native enum layout variant missing type_hash"))?
                    .to_string(),
                tag_value: variant
                    .get("tag_value")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("native enum layout variant missing tag_value"))?,
                payload_offset_bytes: variant
                    .get("payload_offset_bytes")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| {
                        anyhow!("native enum layout variant missing payload_offset_bytes")
                    })?,
            })
        })
        .collect()
}

fn native_debug_metadata(compiled: &CompiledFunction) -> JsonValue {
    json!({
        "schema": NATIVE_DEBUG_METADATA_SCHEMA,
        "text_section": ".text",
        "text_size": compiled.text.len(),
        "ranges": compiled
            .debug_ranges
            .iter()
            .map(|range| {
                json!({
                    "symbol_hash": &range.symbol_hash,
                    "function_def_hash": &range.function_def_hash,
                    "lowered_op_id": &range.lowered_op_id,
                    "value_id": &range.value_id,
                    "lowered_op_kind": &range.lowered_op_kind,
                    "expr_hash": &range.expr_hash,
                    "text_offset_start": range.text_offset_start,
                    "text_offset_end": range.text_offset_end,
                })
            })
            .collect::<Vec<_>>(),
    })
}

#[derive(Debug, Clone)]
struct CompiledFunction {
    text: Vec<u8>,
    relocations: Vec<TextRelocation>,
    debug_ranges: Vec<NativeDebugRange>,
}

#[derive(Debug, Clone)]
struct NativeDebugRange {
    symbol_hash: String,
    function_def_hash: String,
    lowered_op_id: String,
    value_id: String,
    lowered_op_kind: String,
    expr_hash: String,
    text_offset_start: u64,
    text_offset_end: u64,
}

#[derive(Debug, Clone)]
struct TextRelocation {
    offset: u64,
    target_symbol_hash: String,
    target_abi_symbol: String,
    platform: bool,
}

#[derive(Debug)]
struct StackLayout {
    hidden_return_offset: Option<i32>,
    param_offsets: Vec<i32>,
    param_copies: Vec<StackParamCopy<i32>>,
    local_offsets: BTreeMap<usize, i32>,
    value_offsets: BTreeMap<String, i32>,
    stack_size: i32,
}

#[derive(Debug, Clone)]
struct StackParamCopy<T> {
    slot: usize,
    offset: T,
    type_hash: String,
}

struct FoldEmitSpec<'a> {
    id: &'a str,
    target_address: &'a str,
    target_type_hash: &'a str,
    len: &'a str,
    init: &'a str,
    index_slot: usize,
    acc_slot: usize,
    item_slot: usize,
    body: &'a LoweredBlock,
    element_type_hash: &'a str,
    acc_type_hash: &'a str,
}

fn compile_x86_64_function(
    ir: &LoweredFunctionIr,
    function_symbol: &str,
) -> Result<CompiledFunction> {
    let type_layouts = native_type_layouts(ir)?;
    let layout = StackLayout::new(ir)?;
    let mut emitter = FunctionEmitter {
        layout,
        type_layouts,
        text: Vec::new(),
        relocations: Vec::new(),
        active_drop_types: BTreeSet::new(),
        debug_ops: lowered_value_debug_ops(ir)?,
        debug_ranges: Vec::new(),
        symbol_hash: ir.symbol_hash.clone(),
        function_def_hash: ir.function_def_hash.clone(),
    };

    emitter.emit_prologue(ir.params.len())?;
    let (last, body) = ir
        .operations
        .split_last()
        .ok_or_else(|| anyhow!("lowered function has no return"))?;
    emitter.emit_ops(body)?;
    match last {
        LoweredOp::Return { value, type_hash } => {
            if emitter.type_returns_indirect(type_hash)? {
                emitter.emit_aggregate_return(value, type_hash)?;
            } else if type_hash != &type_hash_for("Unit") {
                let offset = emitter.value_offset(value)?;
                emitter.mov_rax_stack(offset);
            }
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
        debug_ranges: emitter.debug_ranges,
    })
}

impl StackLayout {
    fn new(ir: &LoweredFunctionIr) -> Result<Self> {
        let type_layouts = native_type_layouts(ir)?;
        let hidden_return_count = usize::from(native_returns_indirect(
            &type_layouts,
            &ir.return_type_hash,
        )?);
        let mut ids = Vec::new();
        collect_value_ids(&ir.operations, &mut ids)?;
        let mut value_offsets = BTreeMap::new();
        let mut next_offset = (ir.params.len() + hidden_return_count) as i32 * 8;
        let mut param_copies = Vec::new();
        for param in &ir.params {
            if native_passes_indirect(&type_layouts, &param.type_hash)? {
                let size = native_stack_slot_size_bytes(native_type_size(
                    &type_layouts,
                    &param.type_hash,
                )?);
                let size = i32::try_from(size)?;
                let offset = -(next_offset + size);
                param_copies.push(StackParamCopy {
                    slot: param.slot,
                    offset,
                    type_hash: param.type_hash.clone(),
                });
                next_offset += size;
            }
        }
        let mut local_offsets = BTreeMap::new();
        for local in &ir.locals {
            if local.slot != local_offsets.len() {
                bail!("lowered local slots must be sequential");
            }
            let size = i32::try_from(local.size_bytes)?;
            let size = ((size + 7) / 8) * 8;
            let offset = -(next_offset + size);
            local_offsets.insert(local.slot, offset);
            next_offset += size;
        }
        for id in ids {
            let offset = -(next_offset + 8);
            value_offsets.insert(id, offset);
            next_offset += 8;
        }
        let hidden_return_offset = (hidden_return_count == 1).then_some(-8);
        let param_offsets = (0..ir.params.len())
            .map(|idx| -8 * ((idx + hidden_return_count) as i32 + 1))
            .collect::<Vec<_>>();
        let raw_size = next_offset;
        let stack_size = if raw_size == 0 {
            0
        } else {
            ((raw_size + 15) / 16) * 16
        };
        Ok(Self {
            hidden_return_offset,
            param_offsets,
            param_copies,
            local_offsets,
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
            | LoweredOp::ConstUnit { id, .. }
            | LoweredOp::Unary { id, .. }
            | LoweredOp::Binary { id, .. }
            | LoweredOp::Call { id, .. }
            | LoweredOp::BorrowShared { id, .. }
            | LoweredOp::BorrowMut { id, .. }
            | LoweredOp::DerefShared { id, .. }
            | LoweredOp::DerefMut { id, .. }
            | LoweredOp::DerefBox { id, .. }
            | LoweredOp::HeapAlloc { id, .. }
            | LoweredOp::AddrOfParam { id, .. }
            | LoweredOp::AddrOfLocal { id, .. }
            | LoweredOp::AddrOfField { id, .. }
            | LoweredOp::AddrOfEnumPayload { id, .. }
            | LoweredOp::AddrOfIndex { id, .. }
            | LoweredOp::ConstructSlice { id, .. }
            | LoweredOp::SliceLen { id, .. }
            | LoweredOp::SliceData { id, .. }
            | LoweredOp::BoundsCheck { id, .. }
            | LoweredOp::SliceRangeCheck { id, .. }
            | LoweredOp::LoadEnumTag { id, .. }
            | LoweredOp::Load { id, .. }
            | LoweredOp::Copy { id, .. }
            | LoweredOp::Move { id, .. } => push_value_id(ids, seen, id)?,
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
            LoweredOp::Case { id, arms, .. } => {
                push_value_id(ids, seen, id)?;
                for arm in arms {
                    collect_value_ids_inner(&arm.block.operations, ids, seen)?;
                }
            }
            LoweredOp::Fold { id, body, .. } => {
                push_value_id(ids, seen, id)?;
                collect_value_ids_inner(&body.operations, ids, seen)?;
            }
            LoweredOp::Store { .. }
            | LoweredOp::StoreEnumTag { .. }
            | LoweredOp::Drop { .. }
            | LoweredOp::BorrowDebug { .. }
            | LoweredOp::Return { .. } => {}
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
    type_layouts: BTreeMap<String, LoweredTypeLayout>,
    text: Vec<u8>,
    relocations: Vec<TextRelocation>,
    active_drop_types: BTreeSet<String>,
    debug_ops: BTreeMap<String, LoweredDebugOp>,
    debug_ranges: Vec<NativeDebugRange>,
    symbol_hash: String,
    function_def_hash: String,
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
        let arg_shift = usize::from(self.layout.hidden_return_offset.is_some());
        if let Some(offset) = self.layout.hidden_return_offset {
            self.mov_stack_arg_reg(offset, 0)?;
        }
        for slot in 0..param_count {
            self.mov_stack_arg_reg(self.layout.param_offsets[slot], slot + arg_shift)?;
        }
        for copy in self.layout.param_copies.clone() {
            let source_pointer = *self
                .layout
                .param_offsets
                .get(copy.slot)
                .ok_or_else(|| anyhow!("parameter slot out of bounds {}", copy.slot))?;
            self.copy_memory_from_stack_pointer_to_stack(
                copy.offset,
                source_pointer,
                self.type_size(&copy.type_hash)?,
            )?;
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
        let debug_value_id = lowered_op_value_id(op).map(str::to_string);
        let debug_start = self.text.len();
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
            LoweredOp::ConstUnit { id, .. } => {
                self.mov_rax_imm32(0);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::Unary {
                id, kind, value, ..
            } => {
                self.emit_unary(kind, value)?;
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
                target_abi_symbol,
                args,
                return_address,
                ..
            } => {
                let arg_shift = usize::from(return_address.is_some());
                if args.len() + arg_shift > 6 {
                    bail!("native object backend v0 supports at most 6 call arguments");
                }
                if let Some(return_address) = return_address {
                    self.mov_arg_reg_stack(0, self.value_offset(return_address)?)?;
                }
                for (idx, arg) in args.iter().enumerate() {
                    self.mov_arg_reg_stack(idx + arg_shift, self.value_offset(arg)?)?;
                }
                let target_abi_symbol = target_abi_symbol
                    .clone()
                    .unwrap_or(internal_abi_symbol(target_symbol_hash)?);
                let offset = self.text.len() + 1;
                self.text.push(0xe8);
                self.text.extend_from_slice(&[0, 0, 0, 0]);
                self.relocations.push(TextRelocation {
                    offset: offset as u64,
                    target_symbol_hash: target_symbol_hash.clone(),
                    target_abi_symbol,
                    platform: false,
                });
                if let Some(return_address) = return_address {
                    self.mov_rax_stack(self.value_offset(return_address)?);
                }
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
            LoweredOp::Case {
                id,
                scrutinee,
                arms,
                ..
            } => {
                self.emit_case(id, scrutinee, arms)?;
            }
            LoweredOp::Fold {
                id,
                target_address,
                target_type_hash,
                len,
                init,
                index_slot,
                acc_slot,
                item_slot,
                body,
                element_type_hash,
                acc_type_hash,
                ..
            } => {
                let spec = FoldEmitSpec {
                    id,
                    target_address,
                    target_type_hash,
                    len,
                    init,
                    index_slot: *index_slot,
                    acc_slot: *acc_slot,
                    item_slot: *item_slot,
                    body,
                    element_type_hash,
                    acc_type_hash,
                };
                self.emit_fold(spec)?;
            }
            LoweredOp::BorrowShared { id, address, .. }
            | LoweredOp::BorrowMut { id, address, .. } => {
                self.mov_rax_stack(self.value_offset(address)?);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::DerefShared { id, reference, .. }
            | LoweredOp::DerefMut { id, reference, .. } => {
                self.mov_rax_stack(self.value_offset(reference)?);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::DerefBox { id, box_value, .. } => {
                self.mov_rax_stack(self.value_offset(box_value)?);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::HeapAlloc { id, size_bytes, .. } => {
                self.emit_heap_alloc_x86(id, *size_bytes)?;
            }
            LoweredOp::AddrOfParam { id, place } => {
                let LoweredPlace::Param { slot, indirect, .. } = place else {
                    bail!("addr_of_param must contain a param place");
                };
                let offset = *self
                    .layout
                    .param_offsets
                    .get(*slot)
                    .ok_or_else(|| anyhow!("parameter slot out of bounds {slot}"))?;
                if *indirect {
                    let copy_offset = self
                        .layout
                        .param_copies
                        .iter()
                        .find(|copy| copy.slot == *slot)
                        .map(|copy| copy.offset)
                        .ok_or_else(|| {
                            anyhow!("missing indirect parameter copy for slot {slot}")
                        })?;
                    self.lea_rax_stack(copy_offset);
                } else {
                    self.lea_rax_stack(offset);
                }
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::AddrOfLocal { id, place } => {
                let LoweredPlace::Local { slot, .. } = place else {
                    bail!("addr_of_local must contain a local place");
                };
                self.lea_rax_stack(self.local_offset(*slot)?);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::AddrOfField { id, place } => {
                let LoweredPlace::Field {
                    base, offset_bytes, ..
                } = place
                else {
                    bail!("addr_of_field must contain a field place");
                };
                self.mov_rax_stack(self.value_offset(base)?);
                self.add_rax_imm32(i32::try_from(*offset_bytes)?);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::AddrOfEnumPayload { id, place } => {
                let LoweredPlace::EnumPayload {
                    base,
                    payload_offset_bytes,
                    ..
                } = place
                else {
                    bail!("addr_of_enum_payload must contain an enum payload place");
                };
                self.mov_rax_stack(self.value_offset(base)?);
                self.add_rax_imm32(i32::try_from(*payload_offset_bytes)?);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::AddrOfIndex { id, place } => {
                let LoweredPlace::Index {
                    base,
                    index,
                    element_type_hash,
                    ..
                } = place
                else {
                    bail!("addr_of_index must contain an index place");
                };
                self.mov_rax_stack(self.value_offset(base)?);
                self.mov_rcx_stack(self.value_offset(index)?);
                let stride = native_array_stride(&self.type_layouts, element_type_hash)?;
                if stride > 0 {
                    if stride != 1 {
                        self.imul_rcx_imm32(i32::try_from(stride)?);
                    }
                    self.add_rax_rcx();
                }
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::ConstructSlice {
                id,
                address,
                data_address,
                len,
                ..
            } => {
                self.mov_rax_stack(self.value_offset(address)?);
                self.mov_rcx_stack(self.value_offset(data_address)?);
                self.mov_mem_rax_rcx();
                self.mov_rcx_stack(self.value_offset(len)?);
                self.mov_mem_rax_disp_rcx(8);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::SliceLen { id, slice, .. } => {
                self.mov_rax_stack(self.value_offset(slice)?);
                self.mov_rax_mem_rax_disp(8);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::SliceData { id, slice, .. } => {
                self.mov_rax_stack(self.value_offset(slice)?);
                self.mov_rax_mem_rax();
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::BoundsCheck {
                id,
                index,
                len,
                len_value,
                type_hash: _,
            } => {
                self.mov_rcx_stack(self.value_offset(index)?);
                if let Some(len_value) = len_value {
                    self.mov_rdx_stack(self.value_offset(len_value)?);
                    self.cmp_rcx_rdx();
                } else {
                    self.cmp_rcx_imm32(i32::try_from(*len)?);
                }
                let ok = self.emit_jb_placeholder();
                self.emit_ud2();
                self.patch_rel32(ok)?;
                self.mov_rax_imm32(0);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::SliceRangeCheck {
                id,
                start,
                len,
                source_len,
                type_hash: _,
            } => {
                self.mov_rcx_stack(self.value_offset(start)?);
                self.mov_rdx_stack(self.value_offset(source_len)?);
                self.cmp_rcx_rdx();
                let start_ok = self.emit_jbe_placeholder();
                self.emit_ud2();
                self.patch_rel32(start_ok)?;
                self.sub_rdx_rcx();
                self.mov_rax_stack(self.value_offset(len)?);
                self.cmp_rax_rdx();
                let len_ok = self.emit_jbe_placeholder();
                self.emit_ud2();
                self.patch_rel32(len_ok)?;
                self.mov_rax_imm32(0);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::LoadEnumTag {
                id,
                address,
                type_hash: _,
            } => {
                self.mov_rax_stack(self.value_offset(address)?);
                self.mov_rax_mem_rax();
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::StoreEnumTag {
                address, tag_value, ..
            } => {
                self.mov_rax_stack(self.value_offset(address)?);
                self.mov_rcx_imm64(*tag_value);
                self.mov_mem_rax_rcx();
            }
            LoweredOp::Load {
                id,
                address,
                type_hash,
            } => {
                if self.type_passes_indirect(type_hash)? {
                    self.mov_rax_stack(self.value_offset(address)?);
                    self.mov_stack_rax(self.value_offset(id)?);
                } else {
                    self.emit_load_addressed_value_to_stack(id, type_hash, address)?;
                }
            }
            LoweredOp::Store {
                address,
                value,
                type_hash,
            } => {
                if self.type_passes_indirect(type_hash)? {
                    self.copy_memory_from_value_to_address(address, value, type_hash)?;
                } else {
                    self.emit_store_addressed_value(type_hash, address, value)?;
                }
            }
            LoweredOp::Copy {
                id,
                value,
                type_hash: _,
            } => {
                self.mov_rax_stack(self.value_offset(value)?);
                self.mov_stack_rax(self.value_offset(id)?);
            }
            LoweredOp::Move {
                id,
                address,
                type_hash,
            } => {
                if self.type_passes_indirect(type_hash)? {
                    self.mov_rax_stack(self.value_offset(address)?);
                    self.mov_stack_rax(self.value_offset(id)?);
                } else {
                    self.emit_load_addressed_value_to_stack(id, type_hash, address)?;
                }
            }
            LoweredOp::Drop { address, type_hash } => {
                self.mov_rax_stack(self.value_offset(address)?);
                self.emit_drop_ptr_x86(type_hash)?;
            }
            LoweredOp::BorrowDebug { .. } => {}
            LoweredOp::Return { .. } => {
                bail!("return is only valid as the final lowered operation");
            }
        }
        if let Some(value_id) = debug_value_id {
            self.record_debug_range(&value_id, debug_start, self.text.len())?;
        }
        Ok(())
    }

    fn record_debug_range(&mut self, value_id: &str, start: usize, end: usize) -> Result<()> {
        if end <= start {
            return Ok(());
        }
        let op = self
            .debug_ops
            .get(value_id)
            .ok_or_else(|| anyhow!("missing lowered debug op for value id {value_id}"))?;
        self.debug_ranges.push(NativeDebugRange {
            symbol_hash: self.symbol_hash.clone(),
            function_def_hash: self.function_def_hash.clone(),
            lowered_op_id: op.lowered_op_id.clone(),
            value_id: op.value_id.clone(),
            lowered_op_kind: op.lowered_op_kind.clone(),
            expr_hash: op.expr_hash.clone(),
            text_offset_start: start as u64,
            text_offset_end: end as u64,
        });
        Ok(())
    }

    fn type_size(&self, type_hash: &str) -> Result<u64> {
        native_type_size(&self.type_layouts, type_hash)
    }

    fn type_passes_indirect(&self, type_hash: &str) -> Result<bool> {
        native_passes_indirect(&self.type_layouts, type_hash)
    }

    fn type_returns_indirect(&self, type_hash: &str) -> Result<bool> {
        native_returns_indirect(&self.type_layouts, type_hash)
    }

    fn emit_aggregate_return(&mut self, value: &str, type_hash: &str) -> Result<()> {
        let hidden = self
            .layout
            .hidden_return_offset
            .ok_or_else(|| anyhow!("aggregate return missing hidden return slot"))?;
        self.copy_memory_from_stack_pointers(
            hidden,
            self.value_offset(value)?,
            self.type_size(type_hash)?,
        )?;
        self.mov_rax_stack(hidden);
        Ok(())
    }

    fn emit_load_addressed_value(&mut self, type_hash: &str, address: &str) -> Result<()> {
        self.mov_rax_stack(self.value_offset(address)?);
        match self.type_size(type_hash)? {
            0 => self.mov_rax_imm32(0),
            1 => self.movzx_rax_memb_rax(),
            8 => self.mov_rax_mem_rax(),
            size => bail!("native x86_64 backend cannot load scalar size {size}"),
        }
        Ok(())
    }

    fn emit_load_addressed_value_to_stack(
        &mut self,
        id: &str,
        type_hash: &str,
        address: &str,
    ) -> Result<()> {
        let value_offset = self.value_offset(id)?;
        match self.type_size(type_hash)? {
            0 | 1 | 8 => {
                self.emit_load_addressed_value(type_hash, address)?;
                self.mov_stack_rax(value_offset);
            }
            size @ 2..=7 => {
                self.mov_rax_imm32(0);
                self.mov_stack_rax(value_offset);
                self.copy_memory_from_stack_pointer_to_stack(
                    value_offset,
                    self.value_offset(address)?,
                    size,
                )?;
            }
            size => bail!("native x86_64 backend cannot load scalar size {size}"),
        }
        Ok(())
    }

    fn emit_store_addressed_value(
        &mut self,
        type_hash: &str,
        address: &str,
        value: &str,
    ) -> Result<()> {
        match self.type_size(type_hash)? {
            0 => Ok(()),
            1 => {
                self.mov_rax_stack(self.value_offset(address)?);
                self.mov_rcx_stack(self.value_offset(value)?);
                self.mov_memb_rax_cl();
                Ok(())
            }
            size @ 2..=7 => self.copy_memory_from_stack_to_stack_pointer(
                self.value_offset(address)?,
                self.value_offset(value)?,
                size,
            ),
            8 => {
                self.mov_rax_stack(self.value_offset(address)?);
                self.mov_rcx_stack(self.value_offset(value)?);
                self.mov_mem_rax_rcx();
                Ok(())
            }
            size => bail!("native x86_64 backend cannot store scalar size {size}"),
        }
    }

    fn copy_memory_from_value_to_address(
        &mut self,
        address: &str,
        value: &str,
        type_hash: &str,
    ) -> Result<()> {
        self.copy_memory_from_stack_pointers(
            self.value_offset(address)?,
            self.value_offset(value)?,
            self.type_size(type_hash)?,
        )
    }

    fn copy_memory_from_stack_pointer_to_stack(
        &mut self,
        dest_stack_offset: i32,
        source_pointer_offset: i32,
        size_bytes: u64,
    ) -> Result<()> {
        if size_bytes == 0 {
            return Ok(());
        }
        self.lea_rcx_stack(dest_stack_offset);
        self.mov_rax_stack(source_pointer_offset);
        self.copy_memory_from_rax_to_rcx(size_bytes)
    }

    fn copy_memory_from_stack_to_stack_pointer(
        &mut self,
        dest_pointer_offset: i32,
        source_stack_offset: i32,
        size_bytes: u64,
    ) -> Result<()> {
        if size_bytes == 0 {
            return Ok(());
        }
        self.mov_rcx_stack(dest_pointer_offset);
        self.lea_rax_stack(source_stack_offset);
        self.copy_memory_from_rax_to_rcx(size_bytes)
    }

    fn copy_memory_from_stack_pointers(
        &mut self,
        dest_pointer_offset: i32,
        source_pointer_offset: i32,
        size_bytes: u64,
    ) -> Result<()> {
        if size_bytes == 0 {
            return Ok(());
        }
        self.mov_rcx_stack(dest_pointer_offset);
        self.mov_rax_stack(source_pointer_offset);
        self.copy_memory_from_rax_to_rcx(size_bytes)
    }

    fn copy_memory_from_rax_to_rcx(&mut self, size_bytes: u64) -> Result<()> {
        let mut offset = 0_u64;
        while offset + 8 <= size_bytes {
            let offset_i32 = i32::try_from(offset)?;
            self.mov_rdx_mem_rax_disp(offset_i32);
            self.mov_mem_rcx_disp_rdx(offset_i32);
            offset += 8;
        }
        while offset < size_bytes {
            let offset_i32 = i32::try_from(offset)?;
            self.movzx_rdx_memb_rax_disp(offset_i32);
            self.mov_memb_rcx_disp_dl(offset_i32);
            offset += 1;
        }
        Ok(())
    }

    fn emit_unary(&mut self, kind: &str, value: &str) -> Result<()> {
        self.mov_rax_stack(self.value_offset(value)?);
        match kind {
            "neg_i64" => self.text.extend_from_slice(&[0x48, 0xf7, 0xd8]),
            "not_bool" => {
                self.text.extend_from_slice(&[0x48, 0x85, 0xc0]);
                self.text.extend_from_slice(&[0x0f, 0x94, 0xc0]);
                self.text.extend_from_slice(&[0x0f, 0xb6, 0xc0]);
            }
            other => bail!("unsupported lowered unary op for native object backend: {other}"),
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

    fn emit_case(&mut self, id: &str, scrutinee: &str, arms: &[LoweredCaseArm]) -> Result<()> {
        if arms.is_empty() {
            bail!("native x86_64 backend cannot emit empty case");
        }
        let mut end_patches = Vec::new();
        for (idx, arm) in arms.iter().enumerate() {
            if arm.tag_value > i32::MAX as u64 {
                bail!(
                    "native x86_64 backend cannot encode enum tag {}",
                    arm.tag_value
                );
            }
            if idx + 1 < arms.len() {
                self.mov_rax_stack(self.value_offset(scrutinee)?);
                self.mov_rax_mem_rax();
                self.cmp_rax_imm32(i32::try_from(arm.tag_value)?);
                let next_patch = self.emit_jne_placeholder();
                self.emit_ops(&arm.block.operations)?;
                self.mov_rax_stack(self.value_offset(&arm.block.result)?);
                self.mov_stack_rax(self.value_offset(id)?);
                end_patches.push(self.emit_jmp_placeholder());
                self.patch_rel32(next_patch)?;
            } else {
                self.emit_ops(&arm.block.operations)?;
                self.mov_rax_stack(self.value_offset(&arm.block.result)?);
                self.mov_stack_rax(self.value_offset(id)?);
            }
        }
        for patch in end_patches {
            self.patch_rel32(patch)?;
        }
        Ok(())
    }

    fn emit_fold(&mut self, spec: FoldEmitSpec<'_>) -> Result<()> {
        self.store_value_to_local_x86(spec.init, spec.acc_type_hash, spec.acc_slot)?;
        self.mov_rax_imm32(0);
        self.mov_stack_rax(self.local_offset(spec.index_slot)?);

        let loop_start = self.text.len();
        self.mov_rcx_stack(self.local_offset(spec.index_slot)?);
        self.mov_rdx_stack(self.value_offset(spec.len)?);
        self.cmp_rcx_rdx();
        let exit_patch = self.emit_jae_placeholder();

        self.emit_fold_load_item_x86(&spec)?;
        self.emit_ops(&spec.body.operations)?;
        self.store_value_to_local_x86(&spec.body.result, spec.acc_type_hash, spec.acc_slot)?;

        self.mov_rax_stack(self.local_offset(spec.index_slot)?);
        self.add_rax_imm32(1);
        self.mov_stack_rax(self.local_offset(spec.index_slot)?);
        self.emit_jmp_to(loop_start)?;
        self.patch_rel32(exit_patch)?;

        if self.type_passes_indirect(spec.acc_type_hash)? {
            self.lea_rax_stack(self.local_offset(spec.acc_slot)?);
        } else {
            self.mov_rax_stack(self.local_offset(spec.acc_slot)?);
        }
        self.mov_stack_rax(self.value_offset(spec.id)?);
        Ok(())
    }

    fn emit_fold_load_item_x86(&mut self, spec: &FoldEmitSpec<'_>) -> Result<()> {
        self.mov_rax_stack(self.value_offset(spec.target_address)?);
        if native_type_layout(&self.type_layouts, spec.target_type_hash)?.kind == "slice" {
            self.mov_rax_mem_rax();
        }
        self.mov_rcx_stack(self.local_offset(spec.index_slot)?);
        let stride = native_array_stride(&self.type_layouts, spec.element_type_hash)?;
        if stride != 1 {
            self.imul_rcx_imm32(i32::try_from(stride)?);
        }
        self.add_rax_rcx();
        self.store_address_in_rax_to_local_x86(spec.element_type_hash, spec.item_slot)
    }

    fn store_value_to_local_x86(
        &mut self,
        value: &str,
        type_hash: &str,
        slot: usize,
    ) -> Result<()> {
        let local_offset = self.local_offset(slot)?;
        if self.type_passes_indirect(type_hash)? {
            self.copy_memory_from_stack_pointer_to_stack(
                local_offset,
                self.value_offset(value)?,
                self.type_size(type_hash)?,
            )
        } else {
            self.mov_rax_stack(self.value_offset(value)?);
            self.mov_stack_rax(local_offset);
            Ok(())
        }
    }

    fn store_address_in_rax_to_local_x86(&mut self, type_hash: &str, slot: usize) -> Result<()> {
        let local_offset = self.local_offset(slot)?;
        match self.type_size(type_hash)? {
            0 => Ok(()),
            1 => {
                self.movzx_rax_memb_rax();
                self.mov_stack_rax(local_offset);
                Ok(())
            }
            size @ 2..=7 => {
                self.lea_rcx_stack(local_offset);
                self.copy_memory_from_rax_to_rcx(size)
            }
            8 => {
                self.mov_rax_mem_rax();
                self.mov_stack_rax(local_offset);
                Ok(())
            }
            size => {
                self.lea_rcx_stack(local_offset);
                self.copy_memory_from_rax_to_rcx(size)
            }
        }
    }

    fn emit_heap_alloc_x86(&mut self, id: &str, size_bytes: u64) -> Result<()> {
        self.mov_rax_imm64(i64::try_from(size_bytes.max(1))?);
        self.mov_rdi_rax();
        self.emit_platform_call_x86(PLATFORM_MALLOC_SYMBOL_HASH, PLATFORM_MALLOC_ABI_SYMBOL);
        self.cmp_rax_imm32(0);
        let ok = self.emit_jne_placeholder();
        self.emit_ud2();
        self.patch_rel32(ok)?;
        self.mov_stack_rax(self.value_offset(id)?);
        Ok(())
    }

    fn emit_drop_ptr_x86(&mut self, type_hash: &str) -> Result<()> {
        if !native_needs_drop(&self.type_layouts, type_hash)? {
            return Ok(());
        }
        if !self.active_drop_types.insert(type_hash.to_string()) {
            return Ok(());
        }
        let layout = native_type_layout(&self.type_layouts, type_hash)?.clone();
        let result = match layout.kind.as_str() {
            "box" => self.emit_drop_box_ptr_x86(&layout),
            "record" => self.emit_drop_record_ptr_x86(&layout),
            "enum" => self.emit_drop_enum_ptr_x86(&layout),
            "fixed_array" => self.emit_drop_fixed_array_ptr_x86(&layout),
            _ => Ok(()),
        };
        self.active_drop_types.remove(type_hash);
        result
    }

    fn emit_drop_box_ptr_x86(&mut self, layout: &LoweredTypeLayout) -> Result<()> {
        let element_type = native_layout_string(layout, "element_type_hash")?;
        self.mov_rax_mem_rax();
        self.cmp_rax_imm32(0);
        let done = self.emit_jz_placeholder();
        self.sub_rsp_imm8(16);
        self.mov_rsp_rax();
        if native_needs_drop(&self.type_layouts, &element_type)? {
            self.mov_rax_rsp();
            self.emit_drop_ptr_x86(&element_type)?;
        }
        self.mov_rdi_rsp();
        self.add_rsp_imm8(16);
        self.emit_platform_call_x86(PLATFORM_FREE_SYMBOL_HASH, PLATFORM_FREE_ABI_SYMBOL);
        self.patch_rel32(done)?;
        Ok(())
    }

    fn emit_drop_record_ptr_x86(&mut self, layout: &LoweredTypeLayout) -> Result<()> {
        if !native_contains_box(&self.type_layouts, &layout.type_hash)? {
            return Ok(());
        }
        let fields = native_record_fields(layout)?;
        self.sub_rsp_imm8(16);
        self.mov_rsp_rax();
        for field in fields {
            if !native_needs_drop(&self.type_layouts, &field.type_hash)? {
                continue;
            }
            self.mov_rax_rsp();
            self.add_rax_imm32(i32::try_from(field.offset_bytes)?);
            self.emit_drop_ptr_x86(&field.type_hash)?;
        }
        self.add_rsp_imm8(16);
        Ok(())
    }

    fn emit_drop_fixed_array_ptr_x86(&mut self, layout: &LoweredTypeLayout) -> Result<()> {
        let element_type = native_layout_string(layout, "element_type_hash")?;
        if !native_needs_drop(&self.type_layouts, &element_type)? {
            return Ok(());
        }
        let len = native_layout_u64(layout, "len")?;
        let stride = native_layout_u64(layout, "stride_bytes")?;
        self.sub_rsp_imm8(16);
        self.mov_rsp_rax();
        for index in 0..len {
            self.mov_rax_rsp();
            self.add_rax_imm32(i32::try_from(
                index
                    .checked_mul(stride)
                    .ok_or_else(|| anyhow!("native x86_64 array drop offset overflow"))?,
            )?);
            self.emit_drop_ptr_x86(&element_type)?;
        }
        self.add_rsp_imm8(16);
        Ok(())
    }

    fn emit_drop_enum_ptr_x86(&mut self, layout: &LoweredTypeLayout) -> Result<()> {
        if !native_contains_box(&self.type_layouts, &layout.type_hash)? {
            return Ok(());
        }
        let variants = native_enum_variants(layout)?;
        let mut end_patches = Vec::new();
        self.sub_rsp_imm8(16);
        self.mov_rsp_rax();
        for variant in variants {
            if !native_needs_drop(&self.type_layouts, &variant.type_hash)? {
                continue;
            }
            if variant.tag_value > i32::MAX as u64 {
                bail!(
                    "native x86_64 backend cannot encode enum drop tag {}",
                    variant.tag_value
                );
            }
            self.mov_rax_rsp();
            self.mov_rax_mem_rax();
            self.cmp_rax_imm32(i32::try_from(variant.tag_value)?);
            let next_patch = self.emit_jne_placeholder();
            self.mov_rax_rsp();
            self.add_rax_imm32(i32::try_from(variant.payload_offset_bytes)?);
            self.emit_drop_ptr_x86(&variant.type_hash)?;
            end_patches.push(self.emit_jmp_placeholder());
            self.patch_rel32(next_patch)?;
        }
        for patch in end_patches {
            self.patch_rel32(patch)?;
        }
        self.add_rsp_imm8(16);
        Ok(())
    }

    fn emit_platform_call_x86(&mut self, target_symbol_hash: &str, target_abi_symbol: &str) {
        let offset = self.text.len() + 1;
        self.text.push(0xe8);
        self.text.extend_from_slice(&[0, 0, 0, 0]);
        self.relocations.push(TextRelocation {
            offset: offset as u64,
            target_symbol_hash: target_symbol_hash.to_string(),
            target_abi_symbol: target_abi_symbol.to_string(),
            platform: true,
        });
    }

    fn value_offset(&self, id: &str) -> Result<i32> {
        self.layout
            .value_offsets
            .get(id)
            .copied()
            .ok_or_else(|| anyhow!("unknown lowered value id {id}"))
    }

    fn local_offset(&self, slot: usize) -> Result<i32> {
        self.layout
            .local_offsets
            .get(&slot)
            .copied()
            .ok_or_else(|| anyhow!("unknown lowered local slot {slot}"))
    }

    fn mov_rax_imm64(&mut self, value: i64) {
        self.text.extend_from_slice(&[0x48, 0xb8]);
        self.text.extend_from_slice(&(value as u64).to_le_bytes());
    }

    fn mov_rcx_imm64(&mut self, value: u64) {
        self.text.extend_from_slice(&[0x48, 0xb9]);
        self.text.extend_from_slice(&value.to_le_bytes());
    }

    fn mov_rax_imm32(&mut self, value: i32) {
        self.text.push(0xb8);
        self.push_i32(value);
    }

    fn mov_rax_stack(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x85]);
        self.push_i32(offset);
    }

    fn lea_rax_stack(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8d, 0x85]);
        self.push_i32(offset);
    }

    fn lea_rcx_stack(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8d, 0x8d]);
        self.push_i32(offset);
    }

    fn mov_rax_mem_rax(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x00]);
    }

    fn mov_rax_mem_rax_disp(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x80]);
        self.push_i32(offset);
    }

    fn movzx_rax_memb_rax(&mut self) {
        self.text.extend_from_slice(&[0x0f, 0xb6, 0x00]);
    }

    fn add_rax_imm32(&mut self, value: i32) {
        if value == 0 {
            return;
        }
        self.text.extend_from_slice(&[0x48, 0x05]);
        self.push_i32(value);
    }

    fn mov_rcx_stack(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x8d]);
        self.push_i32(offset);
    }

    fn mov_rdx_stack(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x95]);
        self.push_i32(offset);
    }

    fn imul_rcx_imm32(&mut self, value: i32) {
        self.text.extend_from_slice(&[0x48, 0x69, 0xc9]);
        self.push_i32(value);
    }

    fn add_rax_rcx(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x01, 0xc8]);
    }

    fn mov_mem_rax_rcx(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x89, 0x08]);
    }

    fn mov_mem_rax_disp_rcx(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x89, 0x88]);
        self.push_i32(offset);
    }

    fn mov_memb_rax_cl(&mut self) {
        self.text.extend_from_slice(&[0x88, 0x08]);
    }

    fn mov_rdx_mem_rax_disp(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x90]);
        self.push_i32(offset);
    }

    fn mov_mem_rcx_disp_rdx(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x89, 0x91]);
        self.push_i32(offset);
    }

    fn movzx_rdx_memb_rax_disp(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x0f, 0xb6, 0x90]);
        self.push_i32(offset);
    }

    fn mov_memb_rcx_disp_dl(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x88, 0x91]);
        self.push_i32(offset);
    }

    fn mov_stack_rax(&mut self, offset: i32) {
        self.text.extend_from_slice(&[0x48, 0x89, 0x85]);
        self.push_i32(offset);
    }

    fn sub_rsp_imm8(&mut self, value: u8) {
        self.text.extend_from_slice(&[0x48, 0x83, 0xec, value]);
    }

    fn add_rsp_imm8(&mut self, value: u8) {
        self.text.extend_from_slice(&[0x48, 0x83, 0xc4, value]);
    }

    fn mov_rsp_rax(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x89, 0x04, 0x24]);
    }

    fn mov_rax_rsp(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x04, 0x24]);
    }

    fn mov_rdi_rsp(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x8b, 0x3c, 0x24]);
    }

    fn mov_rdi_rax(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x89, 0xc7]);
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

    fn cmp_rax_imm32(&mut self, value: i32) {
        self.text.extend_from_slice(&[0x48, 0x3d]);
        self.push_i32(value);
    }

    fn cmp_rcx_imm32(&mut self, value: i32) {
        self.text.extend_from_slice(&[0x48, 0x81, 0xf9]);
        self.push_i32(value);
    }

    fn cmp_rcx_rdx(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x39, 0xd1]);
    }

    fn cmp_rax_rdx(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x39, 0xd0]);
    }

    fn sub_rdx_rcx(&mut self) {
        self.text.extend_from_slice(&[0x48, 0x29, 0xca]);
    }

    fn emit_jb_placeholder(&mut self) -> usize {
        self.text.extend_from_slice(&[0x0f, 0x82]);
        let patch = self.text.len();
        self.text.extend_from_slice(&[0, 0, 0, 0]);
        patch
    }

    fn emit_jae_placeholder(&mut self) -> usize {
        self.text.extend_from_slice(&[0x0f, 0x83]);
        let patch = self.text.len();
        self.text.extend_from_slice(&[0, 0, 0, 0]);
        patch
    }

    fn emit_jbe_placeholder(&mut self) -> usize {
        self.text.extend_from_slice(&[0x0f, 0x86]);
        let patch = self.text.len();
        self.text.extend_from_slice(&[0, 0, 0, 0]);
        patch
    }

    fn emit_jz_placeholder(&mut self) -> usize {
        self.text.extend_from_slice(&[0x0f, 0x84]);
        let patch = self.text.len();
        self.text.extend_from_slice(&[0, 0, 0, 0]);
        patch
    }

    fn emit_jne_placeholder(&mut self) -> usize {
        self.text.extend_from_slice(&[0x0f, 0x85]);
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

    fn emit_jmp_to(&mut self, target_offset: usize) -> Result<()> {
        self.text.push(0xe9);
        let next = self.text.len() + 4;
        let disp = target_offset as i64 - next as i64;
        let disp = i32::try_from(disp)?;
        self.text.extend_from_slice(&disp.to_le_bytes());
        Ok(())
    }

    fn emit_ud2(&mut self) {
        self.text.extend_from_slice(&[0x0f, 0x0b]);
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
    hidden_return_offset: Option<u32>,
    param_offsets: Vec<u32>,
    param_copies: Vec<StackParamCopy<u32>>,
    local_offsets: BTreeMap<usize, u32>,
    value_offsets: BTreeMap<String, u32>,
    stack_size: u32,
}

fn compile_arm64_function(
    ir: &LoweredFunctionIr,
    function_symbol: &str,
) -> Result<CompiledFunction> {
    let type_layouts = native_type_layouts(ir)?;
    let layout = Arm64StackLayout::new(ir)?;
    let mut emitter = Arm64Emitter {
        layout,
        type_layouts,
        text: Vec::new(),
        relocations: Vec::new(),
        active_drop_types: BTreeSet::new(),
        debug_ops: lowered_value_debug_ops(ir)?,
        debug_ranges: Vec::new(),
        symbol_hash: ir.symbol_hash.clone(),
        function_def_hash: ir.function_def_hash.clone(),
    };

    emitter.emit_prologue(ir.params.len())?;
    let (last, body) = ir
        .operations
        .split_last()
        .ok_or_else(|| anyhow!("lowered function has no return"))?;
    emitter.emit_ops(body)?;
    match last {
        LoweredOp::Return { value, type_hash } => {
            if emitter.type_returns_indirect(type_hash)? {
                emitter.emit_aggregate_return(value, type_hash)?;
            } else if type_hash != &type_hash_for("Unit") {
                let offset = emitter.value_offset(value)?;
                emitter.ldr_stack(0, offset)?;
            }
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
        debug_ranges: emitter.debug_ranges,
    })
}

impl Arm64StackLayout {
    fn new(ir: &LoweredFunctionIr) -> Result<Self> {
        let type_layouts = native_type_layouts(ir)?;
        let hidden_return_count = usize::from(native_returns_indirect(
            &type_layouts,
            &ir.return_type_hash,
        )?);
        let mut ids = Vec::new();
        collect_value_ids(&ir.operations, &mut ids)?;
        let mut value_offsets = BTreeMap::new();
        let mut next_offset = (ir.params.len() + hidden_return_count) as u32 * 8;
        let mut param_copies = Vec::new();
        for param in &ir.params {
            if native_passes_indirect(&type_layouts, &param.type_hash)? {
                let size = native_stack_slot_size_bytes(native_type_size(
                    &type_layouts,
                    &param.type_hash,
                )?);
                let size = u32::try_from(size)?;
                let offset = next_offset;
                param_copies.push(StackParamCopy {
                    slot: param.slot,
                    offset,
                    type_hash: param.type_hash.clone(),
                });
                next_offset += size;
            }
        }
        let mut local_offsets = BTreeMap::new();
        for local in &ir.locals {
            if local.slot != local_offsets.len() {
                bail!("lowered local slots must be sequential");
            }
            let size = u32::try_from(local.size_bytes)?;
            let size = size.div_ceil(8) * 8;
            let offset = next_offset;
            local_offsets.insert(local.slot, offset);
            next_offset += size;
        }
        for id in ids {
            let offset = next_offset;
            value_offsets.insert(id, offset);
            next_offset += 8;
        }
        let hidden_return_offset = (hidden_return_count == 1).then_some(0);
        let param_offsets = (0..ir.params.len())
            .map(|idx| 8 * (idx + hidden_return_count) as u32)
            .collect::<Vec<_>>();
        let raw_size = next_offset;
        let stack_size = if raw_size == 0 {
            0
        } else {
            raw_size.div_ceil(16) * 16
        };
        if stack_size > 4095 {
            bail!("native arm64 backend v0 stack frame is too large");
        }
        Ok(Self {
            hidden_return_offset,
            param_offsets,
            param_copies,
            local_offsets,
            value_offsets,
            stack_size,
        })
    }
}

struct Arm64Emitter {
    layout: Arm64StackLayout,
    type_layouts: BTreeMap<String, LoweredTypeLayout>,
    text: Vec<u8>,
    relocations: Vec<TextRelocation>,
    active_drop_types: BTreeSet<String>,
    debug_ops: BTreeMap<String, LoweredDebugOp>,
    debug_ranges: Vec<NativeDebugRange>,
    symbol_hash: String,
    function_def_hash: String,
}

impl Arm64Emitter {
    fn emit_prologue(&mut self, param_count: usize) -> Result<()> {
        self.emit_u32(0xa9bf7bfd);
        self.emit_u32(0x910003fd);
        if self.layout.stack_size > 0 {
            self.sub_sp_imm(self.layout.stack_size)?;
        }
        let arg_shift = usize::from(self.layout.hidden_return_offset.is_some());
        if let Some(offset) = self.layout.hidden_return_offset {
            self.str_stack(0, offset)?;
        }
        for slot in 0..param_count {
            self.str_stack((slot + arg_shift) as u8, self.layout.param_offsets[slot])?;
        }
        for copy in self.layout.param_copies.clone() {
            let source_pointer = *self
                .layout
                .param_offsets
                .get(copy.slot)
                .ok_or_else(|| anyhow!("parameter slot out of bounds {}", copy.slot))?;
            self.copy_memory_from_stack_pointer_to_stack(
                copy.offset,
                source_pointer,
                self.type_size(&copy.type_hash)?,
            )?;
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
        let debug_value_id = lowered_op_value_id(op).map(str::to_string);
        let debug_start = self.text.len();
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
            LoweredOp::ConstUnit { id, .. } => {
                self.mov_u64(0, 0);
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::Unary {
                id, kind, value, ..
            } => {
                self.emit_unary(kind, value)?;
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
                target_abi_symbol,
                args,
                return_address,
                ..
            } => {
                let arg_shift = usize::from(return_address.is_some());
                if args.len() + arg_shift > 8 {
                    bail!("native arm64 backend v0 supports at most 8 machine call arguments");
                }
                if let Some(return_address) = return_address {
                    self.ldr_stack(0, self.value_offset(return_address)?)?;
                }
                for (idx, arg) in args.iter().enumerate() {
                    self.ldr_stack((idx + arg_shift) as u8, self.value_offset(arg)?)?;
                }
                let target_abi_symbol = macho_symbol_name(
                    &target_abi_symbol
                        .clone()
                        .unwrap_or(internal_abi_symbol(target_symbol_hash)?),
                );
                let offset = self.text.len();
                self.emit_u32(0x94000000);
                self.relocations.push(TextRelocation {
                    offset: offset as u64,
                    target_symbol_hash: target_symbol_hash.clone(),
                    target_abi_symbol,
                    platform: false,
                });
                if let Some(return_address) = return_address {
                    self.ldr_stack(0, self.value_offset(return_address)?)?;
                }
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
            LoweredOp::Case {
                id,
                scrutinee,
                arms,
                ..
            } => {
                self.emit_case(id, scrutinee, arms)?;
            }
            LoweredOp::Fold {
                id,
                target_address,
                target_type_hash,
                len,
                init,
                index_slot,
                acc_slot,
                item_slot,
                body,
                element_type_hash,
                acc_type_hash,
                ..
            } => {
                let spec = FoldEmitSpec {
                    id,
                    target_address,
                    target_type_hash,
                    len,
                    init,
                    index_slot: *index_slot,
                    acc_slot: *acc_slot,
                    item_slot: *item_slot,
                    body,
                    element_type_hash,
                    acc_type_hash,
                };
                self.emit_fold(spec)?;
            }
            LoweredOp::BorrowShared { id, address, .. }
            | LoweredOp::BorrowMut { id, address, .. } => {
                self.ldr_stack(0, self.value_offset(address)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::DerefShared { id, reference, .. }
            | LoweredOp::DerefMut { id, reference, .. } => {
                self.ldr_stack(0, self.value_offset(reference)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::DerefBox { id, box_value, .. } => {
                self.ldr_stack(0, self.value_offset(box_value)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::HeapAlloc { id, size_bytes, .. } => {
                self.emit_heap_alloc_arm64(id, *size_bytes)?;
            }
            LoweredOp::AddrOfParam { id, place } => {
                let LoweredPlace::Param { slot, indirect, .. } = place else {
                    bail!("addr_of_param must contain a param place");
                };
                let offset = *self
                    .layout
                    .param_offsets
                    .get(*slot)
                    .ok_or_else(|| anyhow!("parameter slot out of bounds {slot}"))?;
                if *indirect {
                    let copy_offset = self
                        .layout
                        .param_copies
                        .iter()
                        .find(|copy| copy.slot == *slot)
                        .map(|copy| copy.offset)
                        .ok_or_else(|| {
                            anyhow!("missing indirect parameter copy for slot {slot}")
                        })?;
                    self.add_reg_sp_imm(0, copy_offset)?;
                } else {
                    self.add_reg_sp_imm(0, offset)?;
                }
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::AddrOfLocal { id, place } => {
                let LoweredPlace::Local { slot, .. } = place else {
                    bail!("addr_of_local must contain a local place");
                };
                self.add_reg_sp_imm(0, self.local_offset(*slot)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::AddrOfField { id, place } => {
                let LoweredPlace::Field {
                    base, offset_bytes, ..
                } = place
                else {
                    bail!("addr_of_field must contain a field place");
                };
                self.ldr_stack(0, self.value_offset(base)?)?;
                self.add_reg_imm(0, 0, u32::try_from(*offset_bytes)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::AddrOfEnumPayload { id, place } => {
                let LoweredPlace::EnumPayload {
                    base,
                    payload_offset_bytes,
                    ..
                } = place
                else {
                    bail!("addr_of_enum_payload must contain an enum payload place");
                };
                self.ldr_stack(0, self.value_offset(base)?)?;
                self.add_reg_imm(0, 0, u32::try_from(*payload_offset_bytes)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::AddrOfIndex { id, place } => {
                let LoweredPlace::Index {
                    base,
                    index,
                    element_type_hash,
                    ..
                } = place
                else {
                    bail!("addr_of_index must contain an index place");
                };
                self.ldr_stack(0, self.value_offset(base)?)?;
                self.ldr_stack(1, self.value_offset(index)?)?;
                let stride = native_array_stride(&self.type_layouts, element_type_hash)?;
                if stride > 0 {
                    if stride != 1 {
                        self.mov_u64(2, stride);
                        self.mul_reg(1, 1, 2);
                    }
                    self.add_reg(0, 0, 1);
                }
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::ConstructSlice {
                id,
                address,
                data_address,
                len,
                ..
            } => {
                self.ldr_stack(0, self.value_offset(address)?)?;
                self.ldr_stack(1, self.value_offset(data_address)?)?;
                self.str_reg_addr(1, 0)?;
                self.ldr_stack(1, self.value_offset(len)?)?;
                self.str_reg_addr_offset(1, 0, 8)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::SliceLen { id, slice, .. } => {
                self.ldr_stack(0, self.value_offset(slice)?)?;
                self.ldr_reg_addr_offset(0, 0, 8)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::SliceData { id, slice, .. } => {
                self.ldr_stack(0, self.value_offset(slice)?)?;
                self.ldr_reg_addr(0, 0)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::BoundsCheck {
                id,
                index,
                len,
                len_value,
                type_hash: _,
            } => {
                self.ldr_stack(0, self.value_offset(index)?)?;
                if let Some(len_value) = len_value {
                    self.ldr_stack(1, self.value_offset(len_value)?)?;
                } else {
                    self.mov_u64(1, *len);
                }
                self.cmp_reg(0, 1);
                let ok = self.emit_b_cond_placeholder(3);
                self.emit_u32(0xd4200000);
                self.patch_imm19(ok)?;
                self.mov_u64(0, 0);
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::SliceRangeCheck {
                id,
                start,
                len,
                source_len,
                type_hash: _,
            } => {
                self.ldr_stack(0, self.value_offset(start)?)?;
                self.ldr_stack(1, self.value_offset(source_len)?)?;
                self.cmp_reg(0, 1);
                let start_ok = self.emit_b_cond_placeholder(9);
                self.emit_u32(0xd4200000);
                self.patch_imm19(start_ok)?;
                self.sub_reg(1, 1, 0);
                self.ldr_stack(0, self.value_offset(len)?)?;
                self.cmp_reg(0, 1);
                let len_ok = self.emit_b_cond_placeholder(9);
                self.emit_u32(0xd4200000);
                self.patch_imm19(len_ok)?;
                self.mov_u64(0, 0);
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::LoadEnumTag {
                id,
                address,
                type_hash: _,
            } => {
                self.ldr_stack(0, self.value_offset(address)?)?;
                self.ldr_reg_addr(0, 0)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::StoreEnumTag {
                address, tag_value, ..
            } => {
                self.mov_u64(0, *tag_value);
                self.ldr_stack(1, self.value_offset(address)?)?;
                self.str_reg_addr(0, 1)?;
            }
            LoweredOp::Load {
                id,
                address,
                type_hash,
            } => {
                if self.type_passes_indirect(type_hash)? {
                    self.ldr_stack(0, self.value_offset(address)?)?;
                    self.str_stack(0, self.value_offset(id)?)?;
                } else {
                    self.emit_load_addressed_value_to_stack(id, type_hash, address)?;
                }
            }
            LoweredOp::Store {
                address,
                value,
                type_hash,
            } => {
                if self.type_passes_indirect(type_hash)? {
                    self.copy_memory_from_value_to_address(address, value, type_hash)?;
                } else {
                    self.emit_store_addressed_value(type_hash, address, value)?;
                }
            }
            LoweredOp::Copy {
                id,
                value,
                type_hash: _,
            } => {
                self.ldr_stack(0, self.value_offset(value)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
            LoweredOp::Move {
                id,
                address,
                type_hash,
            } => {
                if self.type_passes_indirect(type_hash)? {
                    self.ldr_stack(0, self.value_offset(address)?)?;
                    self.str_stack(0, self.value_offset(id)?)?;
                } else {
                    self.emit_load_addressed_value_to_stack(id, type_hash, address)?;
                }
            }
            LoweredOp::Drop { address, type_hash } => {
                self.ldr_stack(0, self.value_offset(address)?)?;
                self.emit_drop_ptr_arm64(type_hash)?;
            }
            LoweredOp::BorrowDebug { .. } => {}
            LoweredOp::Return { .. } => {
                bail!("return is only valid as the final lowered operation");
            }
        }
        if let Some(value_id) = debug_value_id {
            self.record_debug_range(&value_id, debug_start, self.text.len())?;
        }
        Ok(())
    }

    fn record_debug_range(&mut self, value_id: &str, start: usize, end: usize) -> Result<()> {
        if end <= start {
            return Ok(());
        }
        let op = self
            .debug_ops
            .get(value_id)
            .ok_or_else(|| anyhow!("missing lowered debug op for value id {value_id}"))?;
        self.debug_ranges.push(NativeDebugRange {
            symbol_hash: self.symbol_hash.clone(),
            function_def_hash: self.function_def_hash.clone(),
            lowered_op_id: op.lowered_op_id.clone(),
            value_id: op.value_id.clone(),
            lowered_op_kind: op.lowered_op_kind.clone(),
            expr_hash: op.expr_hash.clone(),
            text_offset_start: start as u64,
            text_offset_end: end as u64,
        });
        Ok(())
    }

    fn type_size(&self, type_hash: &str) -> Result<u64> {
        native_type_size(&self.type_layouts, type_hash)
    }

    fn type_passes_indirect(&self, type_hash: &str) -> Result<bool> {
        native_passes_indirect(&self.type_layouts, type_hash)
    }

    fn type_returns_indirect(&self, type_hash: &str) -> Result<bool> {
        native_returns_indirect(&self.type_layouts, type_hash)
    }

    fn emit_aggregate_return(&mut self, value: &str, type_hash: &str) -> Result<()> {
        let hidden = self
            .layout
            .hidden_return_offset
            .ok_or_else(|| anyhow!("aggregate return missing hidden return slot"))?;
        self.copy_memory_from_stack_pointers(
            hidden,
            self.value_offset(value)?,
            self.type_size(type_hash)?,
        )?;
        self.ldr_stack(0, hidden)?;
        Ok(())
    }

    fn emit_load_addressed_value(&mut self, type_hash: &str, address: &str) -> Result<()> {
        self.ldr_stack(0, self.value_offset(address)?)?;
        match self.type_size(type_hash)? {
            0 => self.mov_u64(0, 0),
            1 => self.ldrb_reg_addr(0, 0)?,
            8 => self.ldr_reg_addr(0, 0)?,
            size => bail!("native arm64 backend cannot load scalar size {size}"),
        }
        Ok(())
    }

    fn emit_load_addressed_value_to_stack(
        &mut self,
        id: &str,
        type_hash: &str,
        address: &str,
    ) -> Result<()> {
        let value_offset = self.value_offset(id)?;
        match self.type_size(type_hash)? {
            0 | 1 | 8 => {
                self.emit_load_addressed_value(type_hash, address)?;
                self.str_stack(0, value_offset)?;
            }
            size @ 2..=7 => {
                self.mov_u64(0, 0);
                self.str_stack(0, value_offset)?;
                self.copy_memory_from_stack_pointer_to_stack(
                    value_offset,
                    self.value_offset(address)?,
                    size,
                )?;
            }
            size => bail!("native arm64 backend cannot load scalar size {size}"),
        }
        Ok(())
    }

    fn emit_store_addressed_value(
        &mut self,
        type_hash: &str,
        address: &str,
        value: &str,
    ) -> Result<()> {
        match self.type_size(type_hash)? {
            0 => Ok(()),
            1 => {
                self.ldr_stack(0, self.value_offset(value)?)?;
                self.ldr_stack(1, self.value_offset(address)?)?;
                self.strb_reg_addr(0, 1)?;
                Ok(())
            }
            size @ 2..=7 => self.copy_memory_from_stack_to_stack_pointer(
                self.value_offset(address)?,
                self.value_offset(value)?,
                size,
            ),
            8 => {
                self.ldr_stack(0, self.value_offset(value)?)?;
                self.ldr_stack(1, self.value_offset(address)?)?;
                self.str_reg_addr(0, 1)?;
                Ok(())
            }
            size => bail!("native arm64 backend cannot store scalar size {size}"),
        }
    }

    fn copy_memory_from_value_to_address(
        &mut self,
        address: &str,
        value: &str,
        type_hash: &str,
    ) -> Result<()> {
        self.copy_memory_from_stack_pointers(
            self.value_offset(address)?,
            self.value_offset(value)?,
            self.type_size(type_hash)?,
        )
    }

    fn copy_memory_from_stack_pointer_to_stack(
        &mut self,
        dest_stack_offset: u32,
        source_pointer_offset: u32,
        size_bytes: u64,
    ) -> Result<()> {
        if size_bytes == 0 {
            return Ok(());
        }
        self.add_reg_sp_imm(1, dest_stack_offset)?;
        self.ldr_stack(0, source_pointer_offset)?;
        self.copy_memory_from_x0_to_x1(size_bytes)
    }

    fn copy_memory_from_stack_to_stack_pointer(
        &mut self,
        dest_pointer_offset: u32,
        source_stack_offset: u32,
        size_bytes: u64,
    ) -> Result<()> {
        if size_bytes == 0 {
            return Ok(());
        }
        self.ldr_stack(1, dest_pointer_offset)?;
        self.add_reg_sp_imm(0, source_stack_offset)?;
        self.copy_memory_from_x0_to_x1(size_bytes)
    }

    fn copy_memory_from_stack_pointers(
        &mut self,
        dest_pointer_offset: u32,
        source_pointer_offset: u32,
        size_bytes: u64,
    ) -> Result<()> {
        if size_bytes == 0 {
            return Ok(());
        }
        self.ldr_stack(1, dest_pointer_offset)?;
        self.ldr_stack(0, source_pointer_offset)?;
        self.copy_memory_from_x0_to_x1(size_bytes)
    }

    fn copy_memory_from_x0_to_x1(&mut self, size_bytes: u64) -> Result<()> {
        let mut offset = 0_u64;
        while offset + 8 <= size_bytes {
            let offset_u32 = u32::try_from(offset)?;
            self.ldr_reg_addr_offset(2, 0, offset_u32)?;
            self.str_reg_addr_offset(2, 1, offset_u32)?;
            offset += 8;
        }
        while offset < size_bytes {
            let offset_u32 = u32::try_from(offset)?;
            self.ldrb_reg_addr_offset(2, 0, offset_u32)?;
            self.strb_reg_addr_offset(2, 1, offset_u32)?;
            offset += 1;
        }
        Ok(())
    }

    fn emit_unary(&mut self, kind: &str, value: &str) -> Result<()> {
        self.ldr_stack(0, self.value_offset(value)?)?;
        match kind {
            "neg_i64" => self.sub_reg(0, 31, 0),
            "not_bool" => {
                self.cmp_imm_zero(0);
                self.cset(0, 0);
            }
            other => bail!("unsupported lowered unary op for native arm64 backend: {other}"),
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

    fn emit_case(&mut self, id: &str, scrutinee: &str, arms: &[LoweredCaseArm]) -> Result<()> {
        if arms.is_empty() {
            bail!("native arm64 backend cannot emit empty case");
        }
        let mut end_patches = Vec::new();
        for (idx, arm) in arms.iter().enumerate() {
            if idx + 1 < arms.len() {
                self.ldr_stack(0, self.value_offset(scrutinee)?)?;
                self.ldr_reg_addr(0, 0)?;
                self.mov_u64(1, arm.tag_value);
                self.cmp_reg(0, 1);
                let next_patch = self.emit_b_cond_placeholder(1);
                self.emit_ops(&arm.block.operations)?;
                self.ldr_stack(0, self.value_offset(&arm.block.result)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
                end_patches.push(self.emit_b_placeholder());
                self.patch_imm19(next_patch)?;
            } else {
                self.emit_ops(&arm.block.operations)?;
                self.ldr_stack(0, self.value_offset(&arm.block.result)?)?;
                self.str_stack(0, self.value_offset(id)?)?;
            }
        }
        for patch in end_patches {
            self.patch_imm26(patch)?;
        }
        Ok(())
    }

    fn emit_fold(&mut self, spec: FoldEmitSpec<'_>) -> Result<()> {
        self.store_value_to_local_arm64(spec.init, spec.acc_type_hash, spec.acc_slot)?;
        self.mov_u64(0, 0);
        self.str_stack(0, self.local_offset(spec.index_slot)?)?;

        let loop_start = self.text.len();
        self.ldr_stack(0, self.local_offset(spec.index_slot)?)?;
        self.ldr_stack(1, self.value_offset(spec.len)?)?;
        self.cmp_reg(0, 1);
        let exit_patch = self.emit_b_cond_placeholder(2);

        self.emit_fold_load_item_arm64(&spec)?;
        self.emit_ops(&spec.body.operations)?;
        self.store_value_to_local_arm64(&spec.body.result, spec.acc_type_hash, spec.acc_slot)?;

        self.ldr_stack(0, self.local_offset(spec.index_slot)?)?;
        self.mov_u64(1, 1);
        self.add_reg(0, 0, 1);
        self.str_stack(0, self.local_offset(spec.index_slot)?)?;
        self.emit_b_to(loop_start)?;
        self.patch_imm19(exit_patch)?;

        if self.type_passes_indirect(spec.acc_type_hash)? {
            self.add_reg_sp_imm(0, self.local_offset(spec.acc_slot)?)?;
        } else {
            self.ldr_stack(0, self.local_offset(spec.acc_slot)?)?;
        }
        self.str_stack(0, self.value_offset(spec.id)?)?;
        Ok(())
    }

    fn emit_fold_load_item_arm64(&mut self, spec: &FoldEmitSpec<'_>) -> Result<()> {
        self.ldr_stack(0, self.value_offset(spec.target_address)?)?;
        if native_type_layout(&self.type_layouts, spec.target_type_hash)?.kind == "slice" {
            self.ldr_reg_addr(0, 0)?;
        }
        self.ldr_stack(1, self.local_offset(spec.index_slot)?)?;
        let stride = native_array_stride(&self.type_layouts, spec.element_type_hash)?;
        if stride != 1 {
            self.mov_u64(2, stride);
            self.mul_reg(1, 1, 2);
        }
        self.add_reg(0, 0, 1);
        self.store_address_in_x0_to_local_arm64(spec.element_type_hash, spec.item_slot)
    }

    fn store_value_to_local_arm64(
        &mut self,
        value: &str,
        type_hash: &str,
        slot: usize,
    ) -> Result<()> {
        let local_offset = self.local_offset(slot)?;
        if self.type_passes_indirect(type_hash)? {
            self.copy_memory_from_stack_pointer_to_stack(
                local_offset,
                self.value_offset(value)?,
                self.type_size(type_hash)?,
            )
        } else {
            self.ldr_stack(0, self.value_offset(value)?)?;
            self.str_stack(0, local_offset)?;
            Ok(())
        }
    }

    fn store_address_in_x0_to_local_arm64(&mut self, type_hash: &str, slot: usize) -> Result<()> {
        let local_offset = self.local_offset(slot)?;
        match self.type_size(type_hash)? {
            0 => Ok(()),
            1 => {
                self.ldrb_reg_addr(0, 0)?;
                self.str_stack(0, local_offset)?;
                Ok(())
            }
            size @ 2..=7 => {
                self.add_reg_sp_imm(1, local_offset)?;
                self.copy_memory_from_x0_to_x1(size)
            }
            8 => {
                self.ldr_reg_addr(0, 0)?;
                self.str_stack(0, local_offset)?;
                Ok(())
            }
            size => {
                self.add_reg_sp_imm(1, local_offset)?;
                self.copy_memory_from_x0_to_x1(size)
            }
        }
    }

    fn emit_heap_alloc_arm64(&mut self, id: &str, size_bytes: u64) -> Result<()> {
        self.mov_u64(0, size_bytes.max(1));
        self.emit_platform_call_arm64(PLATFORM_MALLOC_SYMBOL_HASH, PLATFORM_MALLOC_ABI_SYMBOL);
        let ok = self.emit_cbnz_placeholder(0);
        self.emit_u32(0xd4200000);
        self.patch_imm19(ok)?;
        self.str_stack(0, self.value_offset(id)?)?;
        Ok(())
    }

    fn emit_drop_ptr_arm64(&mut self, type_hash: &str) -> Result<()> {
        if !native_needs_drop(&self.type_layouts, type_hash)? {
            return Ok(());
        }
        if !self.active_drop_types.insert(type_hash.to_string()) {
            return Ok(());
        }
        let layout = native_type_layout(&self.type_layouts, type_hash)?.clone();
        let result = match layout.kind.as_str() {
            "box" => self.emit_drop_box_ptr_arm64(&layout),
            "record" => self.emit_drop_record_ptr_arm64(&layout),
            "enum" => self.emit_drop_enum_ptr_arm64(&layout),
            "fixed_array" => self.emit_drop_fixed_array_ptr_arm64(&layout),
            _ => Ok(()),
        };
        self.active_drop_types.remove(type_hash);
        result
    }

    fn emit_drop_box_ptr_arm64(&mut self, layout: &LoweredTypeLayout) -> Result<()> {
        let element_type = native_layout_string(layout, "element_type_hash")?;
        self.ldr_reg_addr(0, 0)?;
        let done = self.emit_cbz_placeholder(0);
        self.sub_sp_imm(16)?;
        self.str_stack(0, 0)?;
        if native_needs_drop(&self.type_layouts, &element_type)? {
            self.ldr_stack(0, 0)?;
            self.emit_drop_ptr_arm64(&element_type)?;
        }
        self.ldr_stack(0, 0)?;
        self.add_sp_imm(16)?;
        self.emit_platform_call_arm64(PLATFORM_FREE_SYMBOL_HASH, PLATFORM_FREE_ABI_SYMBOL);
        self.patch_imm19(done)?;
        Ok(())
    }

    fn emit_drop_record_ptr_arm64(&mut self, layout: &LoweredTypeLayout) -> Result<()> {
        if !native_contains_box(&self.type_layouts, &layout.type_hash)? {
            return Ok(());
        }
        let fields = native_record_fields(layout)?;
        self.sub_sp_imm(16)?;
        self.str_stack(0, 0)?;
        for field in fields {
            if !native_needs_drop(&self.type_layouts, &field.type_hash)? {
                continue;
            }
            self.ldr_stack(0, 0)?;
            self.add_reg_imm(0, 0, u32::try_from(field.offset_bytes)?)?;
            self.emit_drop_ptr_arm64(&field.type_hash)?;
        }
        self.add_sp_imm(16)?;
        Ok(())
    }

    fn emit_drop_fixed_array_ptr_arm64(&mut self, layout: &LoweredTypeLayout) -> Result<()> {
        let element_type = native_layout_string(layout, "element_type_hash")?;
        if !native_needs_drop(&self.type_layouts, &element_type)? {
            return Ok(());
        }
        let len = native_layout_u64(layout, "len")?;
        let stride = native_layout_u64(layout, "stride_bytes")?;
        self.sub_sp_imm(16)?;
        self.str_stack(0, 0)?;
        for index in 0..len {
            let offset = index
                .checked_mul(stride)
                .ok_or_else(|| anyhow!("native arm64 array drop offset overflow"))?;
            self.ldr_stack(0, 0)?;
            self.add_reg_imm(0, 0, u32::try_from(offset)?)?;
            self.emit_drop_ptr_arm64(&element_type)?;
        }
        self.add_sp_imm(16)?;
        Ok(())
    }

    fn emit_drop_enum_ptr_arm64(&mut self, layout: &LoweredTypeLayout) -> Result<()> {
        if !native_contains_box(&self.type_layouts, &layout.type_hash)? {
            return Ok(());
        }
        let variants = native_enum_variants(layout)?;
        let mut end_patches = Vec::new();
        self.sub_sp_imm(16)?;
        self.str_stack(0, 0)?;
        for variant in variants {
            if !native_needs_drop(&self.type_layouts, &variant.type_hash)? {
                continue;
            }
            self.ldr_stack(0, 0)?;
            self.ldr_reg_addr(0, 0)?;
            self.mov_u64(1, variant.tag_value);
            self.cmp_reg(0, 1);
            let next_patch = self.emit_b_cond_placeholder(1);
            self.ldr_stack(0, 0)?;
            self.add_reg_imm(0, 0, u32::try_from(variant.payload_offset_bytes)?)?;
            self.emit_drop_ptr_arm64(&variant.type_hash)?;
            end_patches.push(self.emit_b_placeholder());
            self.patch_imm19(next_patch)?;
        }
        for patch in end_patches {
            self.patch_imm26(patch)?;
        }
        self.add_sp_imm(16)?;
        Ok(())
    }

    fn emit_platform_call_arm64(&mut self, target_symbol_hash: &str, target_abi_symbol: &str) {
        let offset = self.text.len();
        self.emit_u32(0x94000000);
        self.relocations.push(TextRelocation {
            offset: offset as u64,
            target_symbol_hash: target_symbol_hash.to_string(),
            target_abi_symbol: macho_symbol_name(target_abi_symbol),
            platform: true,
        });
    }

    fn value_offset(&self, id: &str) -> Result<u32> {
        self.layout
            .value_offsets
            .get(id)
            .copied()
            .ok_or_else(|| anyhow!("unknown lowered value id {id}"))
    }

    fn local_offset(&self, slot: usize) -> Result<u32> {
        self.layout
            .local_offsets
            .get(&slot)
            .copied()
            .ok_or_else(|| anyhow!("unknown lowered local slot {slot}"))
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

    fn add_reg_sp_imm(&mut self, reg: u8, imm: u32) -> Result<()> {
        if reg > 30 {
            bail!("invalid arm64 general register x{reg}");
        }
        if imm > 4095 {
            bail!("arm64 stack address offset too large");
        }
        self.emit_u32(0x910003e0 | (imm << 10) | u32::from(reg));
        Ok(())
    }

    fn add_reg_imm(&mut self, rd: u8, rn: u8, imm: u32) -> Result<()> {
        if rd > 30 || rn > 30 {
            bail!("invalid arm64 general register");
        }
        if imm == 0 {
            return Ok(());
        }
        if imm > 4095 {
            bail!("arm64 register add offset too large");
        }
        self.emit_u32(0x91000000 | (imm << 10) | (u32::from(rn) << 5) | u32::from(rd));
        Ok(())
    }

    fn str_stack(&mut self, reg: u8, offset: u32) -> Result<()> {
        self.stack_mem_op(0xf90003e0, reg, offset)
    }

    fn ldr_stack(&mut self, reg: u8, offset: u32) -> Result<()> {
        self.stack_mem_op(0xf94003e0, reg, offset)
    }

    fn ldr_reg_addr(&mut self, reg: u8, base_reg: u8) -> Result<()> {
        self.reg_mem_op(0xf9400000, reg, base_reg)
    }

    fn str_reg_addr(&mut self, reg: u8, base_reg: u8) -> Result<()> {
        self.reg_mem_op(0xf9000000, reg, base_reg)
    }

    fn ldrb_reg_addr(&mut self, reg: u8, base_reg: u8) -> Result<()> {
        self.reg_byte_mem_op(0x39400000, reg, base_reg)
    }

    fn strb_reg_addr(&mut self, reg: u8, base_reg: u8) -> Result<()> {
        self.reg_byte_mem_op(0x39000000, reg, base_reg)
    }

    fn ldr_reg_addr_offset(&mut self, reg: u8, base_reg: u8, offset: u32) -> Result<()> {
        self.reg_mem_op_offset(0xf9400000, reg, base_reg, offset)
    }

    fn str_reg_addr_offset(&mut self, reg: u8, base_reg: u8, offset: u32) -> Result<()> {
        self.reg_mem_op_offset(0xf9000000, reg, base_reg, offset)
    }

    fn ldrb_reg_addr_offset(&mut self, reg: u8, base_reg: u8, offset: u32) -> Result<()> {
        self.reg_byte_mem_op_offset(0x39400000, reg, base_reg, offset)
    }

    fn strb_reg_addr_offset(&mut self, reg: u8, base_reg: u8, offset: u32) -> Result<()> {
        self.reg_byte_mem_op_offset(0x39000000, reg, base_reg, offset)
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

    fn reg_mem_op(&mut self, base: u32, reg: u8, base_reg: u8) -> Result<()> {
        self.reg_mem_op_offset(base, reg, base_reg, 0)
    }

    fn reg_mem_op_offset(&mut self, base: u32, reg: u8, base_reg: u8, offset: u32) -> Result<()> {
        if reg > 30 || base_reg > 30 {
            bail!("invalid arm64 general register");
        }
        if !offset.is_multiple_of(8) || offset / 8 > 4095 {
            bail!("arm64 address offset cannot be encoded");
        }
        self.emit_u32(base | ((offset / 8) << 10) | (u32::from(base_reg) << 5) | u32::from(reg));
        Ok(())
    }

    fn reg_byte_mem_op(&mut self, base: u32, reg: u8, base_reg: u8) -> Result<()> {
        self.reg_byte_mem_op_offset(base, reg, base_reg, 0)
    }

    fn reg_byte_mem_op_offset(
        &mut self,
        base: u32,
        reg: u8,
        base_reg: u8,
        offset: u32,
    ) -> Result<()> {
        if reg > 30 || base_reg > 30 {
            bail!("invalid arm64 general register");
        }
        if offset > 4095 {
            bail!("arm64 byte address offset cannot be encoded");
        }
        self.emit_u32(base | (offset << 10) | (u32::from(base_reg) << 5) | u32::from(reg));
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

    fn cmp_imm_zero(&mut self, rn: u8) {
        self.emit_u32(0xf100001f | (u32::from(rn) << 5));
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

    fn emit_b_to(&mut self, target_offset: usize) -> Result<()> {
        let source = self.text.len();
        let bytes = target_offset as i64 - source as i64;
        if bytes % 4 != 0 {
            bail!("arm64 branch target is not instruction-aligned");
        }
        let disp = bytes / 4;
        if !(-(1 << 25)..(1 << 25)).contains(&disp) {
            bail!("arm64 branch target out of range");
        }
        let encoded = (disp as i32 as u32) & 0x03ff_ffff;
        self.emit_u32(0x14000000 | encoded);
        Ok(())
    }

    fn emit_b_cond_placeholder(&mut self, cond: u8) -> usize {
        let patch = self.text.len();
        self.emit_u32(0x54000000 | u32::from(cond));
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
