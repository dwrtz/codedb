use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value as JsonValue, json};

use crate::abi::{export_map, internal_abi_symbol, validate_exported_abi_name};
use crate::artifact::CacheKeyInput;
use crate::backend::ArtifactKind;
use crate::backend::native::{
    FrameStats, NativeObjectArtifact, backend_id_for_target, frame_stats, target_frame_limits,
};
use crate::expr::Value;
use crate::jobs::{ArtifactJobClaim, artifact_job_error, new_worker_id};
use crate::lowering::{LoweredFunctionIr, LoweredOp};
use crate::model::{ProgramRootPayload, RootSymbolPayload, qualified_symbol_display};
use crate::store::{
    CodeDb, cache_key_for_input, canonical_json, function_interface_metadata, hash_bytes,
    hash_object_canonical,
};
use crate::types::{TypeSpec, type_hash_for};
use crate::{
    APPLE_ARM64_TARGET, BYTES_DOMAIN, DEFAULT_NATIVE_TARGET, LINUX_X86_64_TARGET, MAIN_BRANCH,
    SCHEMA_VERSION,
};

const LINK_PLAN_SCHEMA: &str = "codedb/link-plan/v1";
const LINK_INPUT_SCHEMA: &str = "codedb/link-input/v1";
const EXECUTABLE_METADATA_SCHEMA: &str = "codedb/executable/v1";
pub(crate) const ENTRY_POINT_METADATA_SCHEMA: &str = "codedb/entry-point/v1";
const LINK_PLAN_BACKEND_ID: &str = "native-link-plan-v0";
const EXTERNAL_CC_LINKER_BACKEND_ID: &str = "external-cc-linker-v0";

pub struct NativeBuild {
    pub executable: Vec<u8>,
    pub cache_key: String,
    pub artifact_hash: String,
}

pub(crate) struct NativeTestHarnessBuild {
    pub(crate) executable: Vec<u8>,
    pub(crate) artifact_hash: String,
    pub(crate) harness_kind: String,
}

struct PreparedLink {
    root_hash: String,
    input_hash: String,
    plan: JsonValue,
    plan_hash: String,
    objects: Vec<PreparedObject>,
}

struct PreparedObject {
    artifact_hash: String,
    cache_key: String,
    bytes: Vec<u8>,
}

struct NativeRecordFieldLayout {
    type_hash: String,
    offset_bytes: u64,
}

struct NativeEnumVariantLayout {
    type_hash: String,
    tag_value: u64,
    payload_offset_bytes: u64,
}

struct PlannedLink {
    input: JsonValue,
    input_hash: String,
    link_plan_cache_key: String,
    link_plan_key_input: CacheKeyInput,
    target_triple: String,
    entry_symbol_hash: String,
    entry_abi_symbol: String,
    entry_effects: Vec<String>,
    entry_point: JsonValue,
    objects: Vec<PlannedObject>,
    external_symbols: Vec<PlannedExternalSymbol>,
    platform_external_symbols: Vec<PlannedPlatformExternalSymbol>,
    capabilities: Vec<PlannedCapability>,
    export_map: Vec<JsonValue>,
    link_options: JsonValue,
}

struct PlannedObject {
    symbol_hash: String,
    definition_hash: String,
    signature_hash: String,
    param_type_hashes: Vec<String>,
    return_type_hash: String,
    effects: Vec<String>,
    internal_abi_symbol: String,
    object_cache_key: String,
    object_key_input: CacheKeyInput,
}

struct PlannedExternalSymbol {
    symbol_hash: String,
    definition_hash: String,
    signature_hash: String,
    param_type_hashes: Vec<String>,
    return_type_hash: String,
    effects: Vec<String>,
    abi: String,
    link_name: String,
    library: Option<String>,
}

#[derive(Debug, Clone)]
struct PlannedPlatformExternalSymbol {
    symbol_hash: String,
    link_name: String,
    source: String,
}

#[derive(Debug, Clone)]
struct PlannedCapability {
    name: String,
    source: String,
    symbol_hash: String,
    effects: Vec<String>,
}

fn planned_platform_external_symbol_entry(symbol: &PlannedPlatformExternalSymbol) -> JsonValue {
    json!({
        "symbol_hash": &symbol.symbol_hash,
        "link_name": &symbol.link_name,
        "platform": true,
        "source": &symbol.source,
    })
}

impl PlannedLink {
    fn job_cache_keys(&self) -> Vec<String> {
        self.objects
            .iter()
            .map(|object| object.object_cache_key.clone())
            .chain(std::iter::once(self.link_plan_cache_key.clone()))
            .collect()
    }

    fn object_job_entries(&self) -> Vec<JsonValue> {
        self.objects
            .iter()
            .map(|object| {
                json!({
                    "symbol_hash": &object.symbol_hash,
                    "definition_hash": &object.definition_hash,
                    "signature_hash": &object.signature_hash,
                    "param_type_hashes": &object.param_type_hashes,
                    "return_type_hash": &object.return_type_hash,
                    "effects": &object.effects,
                    "internal_abi_symbol": &object.internal_abi_symbol,
                    "object_cache_key": &object.object_cache_key,
                })
            })
            .collect()
    }

    fn external_symbol_entries(&self) -> Vec<JsonValue> {
        self.external_symbols
            .iter()
            .map(|symbol| {
                json!({
                    "symbol_hash": &symbol.symbol_hash,
                    "definition_hash": &symbol.definition_hash,
                    "signature_hash": &symbol.signature_hash,
                    "param_type_hashes": &symbol.param_type_hashes,
                    "return_type_hash": &symbol.return_type_hash,
                    "effects": &symbol.effects,
                    "abi": &symbol.abi,
                    "link_name": &symbol.link_name,
                    "library": &symbol.library,
                })
            })
            .collect()
    }

    fn platform_external_symbol_entries(&self) -> Vec<JsonValue> {
        self.platform_external_symbols
            .iter()
            .map(planned_platform_external_symbol_entry)
            .collect()
    }

    fn capability_entries(&self) -> Vec<JsonValue> {
        self.capabilities
            .iter()
            .map(|capability| {
                json!({
                    "name": &capability.name,
                    "source": &capability.source,
                    "symbol_hash": &capability.symbol_hash,
                    "effects": &capability.effects,
                })
            })
            .collect()
    }
}

impl CodeDb {
    pub fn link_plan_main_branch(
        &mut self,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<String> {
        let prepared = self.prepare_link_plan_main_branch(entry_name, target_triple)?;
        Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&prepared.plan)?
        ))
    }

    pub fn build_plan_main_branch(
        &mut self,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<String> {
        self.build_plan_branch(MAIN_BRANCH, entry_name, target_triple)
    }

    pub(crate) fn build_plan_branch(
        &mut self,
        branch_name: &str,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<String> {
        let planned = self.plan_link_jobs_branch(branch_name, entry_name, target_triple)?;
        self.ensure_planned_artifact_jobs(&planned)?;
        let jobs = self.artifact_job_json_for_cache_keys(&planned.job_cache_keys())?;
        let link_plan_hash = self
            .lookup_cache(&planned.link_plan_key_input)?
            .map(|entry| entry.artifact_hash);
        let payload = json!({
            "schema": "codedb/native-build-plan/v1",
            "branch": branch_name,
            "planned": true,
            "executes_artifacts": false,
            "target_triple": &planned.target_triple,
            "entry_symbol_hash": &planned.entry_symbol_hash,
            "entry_abi_symbol": &planned.entry_abi_symbol,
            "entry_effects": &planned.entry_effects,
            "entry_point": &planned.entry_point,
            "link_plan_input_hash": &planned.input_hash,
            "link_plan_cache_key": &planned.link_plan_cache_key,
            "link_plan_hash": link_plan_hash,
            "artifact_kinds": ["object_file", "link_plan", "executable"],
            "jobs": jobs,
            "objects": planned.object_job_entries(),
            "export_map": &planned.export_map,
            "external_symbols": planned.external_symbol_entries(),
            "platform_external_symbols": planned.platform_external_symbol_entries(),
            "capabilities": planned.capability_entries(),
            "link_options": &planned.link_options,
        });
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn build_main_branch(
        &mut self,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<NativeBuild> {
        self.build_branch(MAIN_BRANCH, entry_name, target_triple)
    }

    pub(crate) fn build_branch(
        &mut self,
        branch_name: &str,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<NativeBuild> {
        let prepared = self.prepare_link_plan_branch(branch_name, entry_name, target_triple)?;
        self.ensure_executable_entry(&prepared)?;

        let linker_identity = host_linker_identity_for_target(target_triple)?;
        let linker_identity_hash = hash_bytes(BYTES_DOMAIN, linker_identity.as_bytes());
        let key_input = executable_cache_key(&prepared, &linker_identity_hash);
        let cache_key = cache_key_for_input(&key_input)?;
        if let Some(cache_entry) = self.lookup_cache(&key_input)?
            && let Some(bytes) = cache_entry.artifact_bytes
        {
            return Ok(NativeBuild {
                executable: bytes,
                cache_key,
                artifact_hash: cache_entry.artifact_hash,
            });
        }

        let worker_id = new_worker_id("executable");
        match self.claim_artifact_job(&cache_key, ArtifactKind::Executable, &worker_id)? {
            ArtifactJobClaim::Succeeded
            | ArtifactJobClaim::Busy {
                status: _,
                worker_id: _,
            } => {
                let cache_entry = self.wait_for_artifact_cache(&key_input, &cache_key)?;
                let bytes = cache_entry
                    .artifact_bytes
                    .ok_or_else(|| anyhow!("executable cache entry missing artifact_bytes"))?;
                return Ok(NativeBuild {
                    executable: bytes,
                    cache_key,
                    artifact_hash: cache_entry.artifact_hash,
                });
            }
            ArtifactJobClaim::Claimed => {}
        }

        let executable = match link_with_cc(&prepared) {
            Ok(executable) => executable,
            Err(err) => {
                let _ = self.fail_artifact_job(
                    &cache_key,
                    &worker_id,
                    &artifact_job_error("link_failed", format!("{err:#}")),
                );
                return Err(err);
            }
        };
        let artifact_hash = hash_bytes(BYTES_DOMAIN, &executable);
        let metadata = json!({
            "schema": EXECUTABLE_METADATA_SCHEMA,
            "target_triple": target_triple,
            "entry_symbol_hash": prepared.plan["entry_symbol_hash"].clone(),
            "entry_abi_symbol": prepared.plan["entry_abi_symbol"].clone(),
            "entry_point": prepared.plan["entry_point"].clone(),
            "link_plan_hash": prepared.plan_hash,
            "linker": "cc",
            "linker_identity_hash": linker_identity_hash,
            "object_artifact_hashes": prepared.objects
                .iter()
                .map(|object| object.artifact_hash.clone())
                .collect::<Vec<_>>(),
            "object_cache_keys": prepared.objects
                .iter()
                .map(|object| object.cache_key.clone())
                .collect::<Vec<_>>(),
        });
        if let Err(err) = self.write_cache_bytes(key_input, &metadata, &executable) {
            let _ = self.fail_artifact_job(
                &cache_key,
                &worker_id,
                &artifact_job_error("cache_write_failed", format!("{err:#}")),
            );
            return Err(err);
        }
        self.complete_artifact_job(&cache_key, &worker_id)?;
        Ok(NativeBuild {
            executable,
            cache_key,
            artifact_hash,
        })
    }

    pub(crate) fn build_native_test_harness_branch(
        &mut self,
        branch_name: &str,
        entry_symbol: &str,
        expected: &Value,
        target_triple: &str,
    ) -> Result<NativeTestHarnessBuild> {
        self.ensure_initialized()?;
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let prepared =
            self.prepare_link_plan(&branch.root_hash, &root, entry_symbol, target_triple)?;
        let harness = self.native_test_harness_source(
            &root,
            &prepared,
            entry_symbol,
            expected,
            target_triple,
        )?;
        let executable = link_with_cc_harness(&prepared, &harness)?;
        let artifact_hash = hash_bytes(BYTES_DOMAIN, &executable);
        Ok(NativeTestHarnessBuild {
            executable,
            artifact_hash,
            harness_kind: "c-main-compare-aggregate-return".to_string(),
        })
    }

    /// Build an executable whose `main` prints the scalar (i64/u8/bool) result
    /// of the entry to stdout as a full-width decimal integer, then exits 0.
    /// The caller parses and compares the printed value, so the comparison is
    /// exact over the whole i64 range and never aliases through the 8-bit
    /// process exit status (unlike encoding the result in the exit code).
    pub(crate) fn build_native_scalar_test_harness_branch(
        &mut self,
        branch_name: &str,
        entry_symbol: &str,
        target_triple: &str,
    ) -> Result<NativeTestHarnessBuild> {
        self.ensure_initialized()?;
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let prepared =
            self.prepare_link_plan(&branch.root_hash, &root, entry_symbol, target_triple)?;
        let root_entry = self
            .root_symbol(&root, entry_symbol)
            .ok_or_else(|| anyhow!("entry symbol missing from root {entry_symbol}"))?;
        let (params, return_type_hash) = self.signature_parts(&root_entry.signature)?;
        if !params.is_empty() {
            bail!("native scalar harness entry must not take parameters");
        }
        match self.type_spec_in_root(&root, &return_type_hash)? {
            TypeSpec::Builtin(kind)
                if crate::types::scalar_int_type(&kind).is_some() || kind == "Bool" => {}
            _ => bail!("native scalar harness entry must return a sized integer or bool"),
        }
        let entry_abi_symbol = prepared
            .plan
            .get("entry_abi_symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("link plan missing entry ABI symbol"))?;
        let export_wrappers = export_wrapper_source(&prepared.plan)?;
        let harness = format!(
            "{export_wrappers}#include <stdio.h>\n{ARGV_RUNTIME_SOURCE}long {entry_abi_symbol}(void);\nint main(void) {{ printf(\"%lld\\n\", (long long){entry_abi_symbol}()); return 0; }}\n"
        );
        let executable = link_with_cc_harness(&prepared, &harness)?;
        let artifact_hash = hash_bytes(BYTES_DOMAIN, &executable);
        Ok(NativeTestHarnessBuild {
            executable,
            artifact_hash,
            harness_kind: "c-main-print-scalar".to_string(),
        })
    }

    fn prepare_link_plan_main_branch(
        &mut self,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<PreparedLink> {
        self.prepare_link_plan_branch(MAIN_BRANCH, entry_name, target_triple)
    }

    fn prepare_link_plan_branch(
        &mut self,
        branch_name: &str,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<PreparedLink> {
        self.ensure_initialized()?;
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let entry_symbol = self
            .resolve_symbol_or_name(&branch.root_hash, entry_name)
            .map_err(|err| anyhow!("unknown entry function {entry_name}: {err}"))?;
        self.prepare_link_plan(&branch.root_hash, &root, &entry_symbol, target_triple)
    }

    fn prepare_link_plan(
        &mut self,
        root_hash: &str,
        root: &ProgramRootPayload,
        entry_symbol: &str,
        target_triple: &str,
    ) -> Result<PreparedLink> {
        let planned = self.plan_link_jobs(root_hash, root, entry_symbol, target_triple)?;
        let symbols = planned
            .objects
            .iter()
            .map(|object| object.symbol_hash.clone())
            .collect::<Vec<_>>();
        let native_objects =
            self.emit_objects_for_symbols_parallel(root_hash, &symbols, target_triple)?;
        let mut objects = Vec::new();
        let mut object_entries = Vec::new();
        for (planned_object, object) in planned.objects.iter().zip(native_objects.into_iter()) {
            let root_entry = self
                .root_symbol(root, &planned_object.symbol_hash)
                .ok_or_else(|| {
                    anyhow!(
                        "link plan symbol missing from root {}",
                        planned_object.symbol_hash
                    )
                })?;
            if root_entry.definition != planned_object.definition_hash
                || root_entry.signature != planned_object.signature_hash
            {
                bail!(
                    "planned link object {} no longer matches the root",
                    planned_object.symbol_hash
                );
            }
            let object_metadata = object.metadata.clone();
            object_entries.push(json!({
                "symbol_hash": &planned_object.symbol_hash,
                "definition_hash": &planned_object.definition_hash,
                "signature_hash": &planned_object.signature_hash,
                "param_type_hashes": &planned_object.param_type_hashes,
                "return_type_hash": &planned_object.return_type_hash,
                "internal_abi_symbol": &planned_object.internal_abi_symbol,
                "defined_symbols": required_metadata_value(&object_metadata, "defined_symbols")?,
                "object_symbols": object_metadata
                    .get("object_symbols")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
                "object_format": required_metadata_str(&object_metadata, "object_format")?,
                "object_artifact_hash": &object.artifact_hash,
                "object_cache_key": &object.cache_key,
                "called_symbols": required_metadata_value(&object_metadata, "called_symbols")?,
                "relocations": required_metadata_value(&object_metadata, "relocations")?,
                "debug_metadata": required_metadata_value(&object_metadata, "debug_metadata")?,
                "static_data": required_metadata_value(&object_metadata, "static_data")?,
            }));
            objects.push(prepared_object(object));
        }

        let input_hash = self.put_object("LinkPlanInput", &planned.input)?;
        if input_hash != planned.input_hash {
            bail!("computed link input hash does not match planned link input hash");
        }
        let platform_external_symbols = merge_platform_external_symbol_entries(
            platform_external_symbols_from_objects(&object_entries)?,
            planned.platform_external_symbol_entries(),
        );
        let mut plan = json!({
            "schema": LINK_PLAN_SCHEMA,
            "input_hash": &input_hash,
            "target_triple": &planned.target_triple,
            "entry_symbol_hash": &planned.entry_symbol_hash,
            "entry_abi_symbol": &planned.entry_abi_symbol,
            "entry_point": planned.entry_point.clone(),
            "objects": object_entries,
            "export_map": planned.export_map.clone(),
            "external_symbols": planned.external_symbol_entries(),
            "platform_external_symbols": platform_external_symbols,
            "output_kind": planned.input["output_kind"].clone(),
            "link_options": planned.link_options.clone(),
        });
        let key_input = planned.link_plan_key_input;
        let plan_cache_key = planned.link_plan_cache_key;
        let plan_hash;
        if let Some(cache_entry) = self.lookup_cache(&key_input)?
            && let Some(artifact_json) = cache_entry.artifact_json
        {
            let cached_plan = json_metadata(&artifact_json)?;
            if cached_plan != plan {
                bail!("cached link plan does not match recomputed link plan");
            }
            plan = cached_plan;
            plan_hash = cache_entry.artifact_hash;
        } else {
            let worker_id = new_worker_id("link-plan");
            match self.claim_artifact_job(&plan_cache_key, ArtifactKind::LinkPlan, &worker_id)? {
                ArtifactJobClaim::Claimed => {
                    plan_hash = match self.write_cache_json_for_key(key_input.clone(), &plan) {
                        Ok(plan_hash) => plan_hash,
                        Err(err) => {
                            let _ = self.fail_artifact_job(
                                &plan_cache_key,
                                &worker_id,
                                &artifact_job_error("cache_write_failed", format!("{err:#}")),
                            );
                            return Err(err);
                        }
                    };
                    self.complete_artifact_job(&plan_cache_key, &worker_id)?;
                }
                ArtifactJobClaim::Succeeded
                | ArtifactJobClaim::Busy {
                    status: _,
                    worker_id: _,
                } => {
                    let cache_entry = self.wait_for_artifact_cache(&key_input, &plan_cache_key)?;
                    let artifact_json = cache_entry
                        .artifact_json
                        .ok_or_else(|| anyhow!("link plan cache entry missing artifact_json"))?;
                    let cached_plan = json_metadata(&artifact_json)?;
                    if cached_plan != plan {
                        bail!("cached link plan does not match recomputed link plan");
                    }
                    plan = cached_plan;
                    plan_hash = cache_entry.artifact_hash;
                }
            }
        }

        Ok(PreparedLink {
            root_hash: root_hash.to_string(),
            input_hash,
            plan,
            plan_hash,
            objects,
        })
    }

    fn plan_link_jobs_branch(
        &mut self,
        branch_name: &str,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<PlannedLink> {
        self.ensure_initialized()?;
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let entry_symbol = self
            .resolve_symbol_or_name(&branch.root_hash, entry_name)
            .map_err(|err| anyhow!("unknown entry function {entry_name}: {err}"))?;
        self.plan_link_jobs(&branch.root_hash, &root, &entry_symbol, target_triple)
    }

    fn plan_link_jobs(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        entry_symbol: &str,
        target_triple: &str,
    ) -> Result<PlannedLink> {
        let symbols = self.reachable_symbols(root_hash, entry_symbol)?;
        let backend_id = backend_id_for_target(target_triple)?;
        let mut objects = Vec::new();
        let mut external_symbols = Vec::new();
        let mut platform_external_symbols = BTreeMap::new();
        let mut capabilities = BTreeMap::new();
        for symbol in symbols {
            let root_entry = self
                .root_symbol(root, &symbol)
                .ok_or_else(|| anyhow!("link plan symbol missing from root {symbol}"))?;
            let (param_type_hashes, return_type_hash) =
                self.signature_parts(&root_entry.signature)?;
            let effects = self.signature_effect_names(&root_entry.signature)?;
            self.collect_symbol_capabilities(
                root,
                &symbol,
                root_entry,
                &effects,
                &mut capabilities,
            )?;
            if self.definition_is_external(&root_entry.definition)? {
                let external = self.external_function_metadata(&root_entry.definition)?;
                collect_semantic_platform_external(
                    root,
                    &symbol,
                    &external.link_name,
                    &mut platform_external_symbols,
                )?;
                external_symbols.push(PlannedExternalSymbol {
                    symbol_hash: symbol.clone(),
                    definition_hash: root_entry.definition.clone(),
                    signature_hash: root_entry.signature.clone(),
                    param_type_hashes,
                    return_type_hash,
                    effects,
                    abi: external.abi,
                    link_name: external.link_name,
                    library: external.library,
                });
                continue;
            }
            let mut dependency_interface_hashes = self
                .dependencies_for_definition(root, &root_entry.definition)?
                .into_iter()
                .map(|dependency| {
                    let entry = self
                        .root_symbol(root, &dependency)
                        .ok_or_else(|| anyhow!("native object dependency missing {dependency}"))?;
                    let metadata = function_interface_metadata(&entry.symbol, &entry.signature)?;
                    Ok(hash_bytes(
                        BYTES_DOMAIN,
                        canonical_json(&metadata).as_bytes(),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            dependency_interface_hashes.sort();
            dependency_interface_hashes.dedup();
            let lowered = self.build_lowered_function_ir(root, root_entry, target_triple)?;
            let compiler_platform_usage = compiler_platform_usage_for_ir(&lowered);
            collect_compiler_platform_externals(&lowered, &mut platform_external_symbols);
            // Compiler-generated box allocation/drop glue uses the platform
            // allocator without a direct `malloc`/`free` call in the function
            // body, so the direct-dependency heuristic in
            // `collect_symbol_capabilities` misses it. Tag the `alloc` capability
            // here too, so a box-allocating function's capability accounting
            // matches its declared `alloc` effect and the malloc/free it links.
            if (compiler_platform_usage.uses_malloc || compiler_platform_usage.uses_free)
                && effects.iter().any(|effect| effect == "alloc")
                && let Some(source) = qualified_symbol_display(root, &symbol)
            {
                capabilities
                    .entry(format!("alloc:{symbol}"))
                    .or_insert(PlannedCapability {
                        name: "alloc".to_string(),
                        source,
                        symbol_hash: symbol.clone(),
                        effects: effects.clone(),
                    });
            }
            // Process-argument reads (R12) call the harness argv runtime;
            // surface them as the `args` capability, like the box-allocation
            // `alloc` tagging above.
            if compiler_platform_usage.uses_args
                && let Some(source) = qualified_symbol_display(root, &symbol)
            {
                capabilities
                    .entry(format!("args:{symbol}"))
                    .or_insert(PlannedCapability {
                        name: "args".to_string(),
                        source,
                        symbol_hash: symbol.clone(),
                        effects: effects.clone(),
                    });
            }
            let dependency_implementation_hashes =
                self.native_object_type_dependency_hashes(root, &lowered, target_triple)?;
            let object_key_input = CacheKeyInput::new(
                ArtifactKind::ObjectFile,
                &root_entry.definition,
                backend_id,
                target_triple,
            )
            .with_dependency_interface_hashes(dependency_interface_hashes)
            .with_dependency_implementation_hashes(dependency_implementation_hashes);
            let object_cache_key = cache_key_for_input(&object_key_input)?;
            objects.push(PlannedObject {
                symbol_hash: symbol.clone(),
                definition_hash: root_entry.definition.clone(),
                signature_hash: root_entry.signature.clone(),
                param_type_hashes,
                return_type_hash,
                effects,
                internal_abi_symbol: internal_abi_symbol(&symbol)?,
                object_cache_key,
                object_key_input,
            });
        }

        let linked_internal_symbols = objects
            .iter()
            .map(|object| object.symbol_hash.clone())
            .collect::<BTreeSet<_>>();
        let export_map = export_map(root)?
            .into_iter()
            .filter(|export| linked_internal_symbols.contains(&export.symbol))
            .map(|export| {
                json!({
                    "symbol_hash": export.symbol,
                    "internal_abi_symbol": export.internal_abi_symbol,
                    "exported_abi_symbol": export.exported_name,
                })
            })
            .collect::<Vec<_>>();
        let link_options = link_options(target_triple)?;
        let object_cache_keys = objects
            .iter()
            .map(|object| object.object_cache_key.clone())
            .collect::<Vec<_>>();
        let external_symbol_entries = external_symbols
            .iter()
            .map(|symbol| {
                json!({
                    "symbol_hash": &symbol.symbol_hash,
                    "definition_hash": &symbol.definition_hash,
                    "signature_hash": &symbol.signature_hash,
                    "param_type_hashes": &symbol.param_type_hashes,
                    "return_type_hash": &symbol.return_type_hash,
                    "effects": &symbol.effects,
                    "abi": &symbol.abi,
                    "link_name": &symbol.link_name,
                    "library": &symbol.library,
                })
            })
            .collect::<Vec<_>>();
        let platform_external_symbol_entries = platform_external_symbols
            .values()
            .map(planned_platform_external_symbol_entry)
            .collect::<Vec<_>>();
        let entry = root
            .symbols
            .iter()
            .find(|entry| entry.symbol == entry_symbol)
            .ok_or_else(|| anyhow!("entry symbol missing from root {entry_symbol}"))?;
        let entry_abi_symbol = self.abi_symbol_for_entry(entry)?;
        let entry_param_type_hashes;
        let entry_return_type_hash;
        {
            let (params, return_type) = self.signature_parts(&entry.signature)?;
            entry_param_type_hashes = params;
            entry_return_type_hash = return_type;
        }
        let entry_effects = self.signature_effect_names(&entry.signature)?;
        let entry_point = native_process_entry_metadata(
            entry_symbol,
            &entry_abi_symbol,
            &entry_param_type_hashes,
            &entry_return_type_hash,
            &entry_effects,
        );
        let input = json!({
            "schema": LINK_INPUT_SCHEMA,
            "target_triple": target_triple,
            "entry_symbol_hash": entry_symbol,
            "entry_abi_symbol": &entry_abi_symbol,
            "entry_point": &entry_point,
            "object_cache_keys": &object_cache_keys,
            "external_symbols": &external_symbol_entries,
            "platform_external_symbols": &platform_external_symbol_entries,
            "export_map": &export_map,
            "output_kind": "executable",
            "link_options": &link_options,
        });
        let input_hash =
            hash_object_canonical("LinkPlanInput", SCHEMA_VERSION, &canonical_json(&input));
        let link_plan_key_input = CacheKeyInput::new(
            ArtifactKind::LinkPlan,
            &input_hash,
            LINK_PLAN_BACKEND_ID,
            target_triple,
        )
        .with_dependency_implementation_hashes(object_cache_keys);
        let link_plan_cache_key = cache_key_for_input(&link_plan_key_input)?;

        Ok(PlannedLink {
            input,
            input_hash,
            link_plan_cache_key,
            link_plan_key_input,
            target_triple: target_triple.to_string(),
            entry_symbol_hash: entry_symbol.to_string(),
            entry_abi_symbol,
            entry_effects,
            entry_point,
            objects,
            external_symbols,
            platform_external_symbols: platform_external_symbols.into_values().collect(),
            capabilities: capabilities.into_values().collect(),
            export_map,
            link_options,
        })
    }

    fn collect_symbol_capabilities(
        &self,
        root: &ProgramRootPayload,
        symbol_hash: &str,
        entry: &RootSymbolPayload,
        effects: &[String],
        out: &mut BTreeMap<String, PlannedCapability>,
    ) -> Result<()> {
        if self.definition_is_external(&entry.definition)? {
            return Ok(());
        }
        let Some(source) = qualified_symbol_display(root, symbol_hash) else {
            return Ok(());
        };
        let dependency_link_names =
            self.external_link_names_for_direct_dependencies(root, &entry.definition)?;
        let has_io = effects.iter().any(|effect| effect == "io");
        let has_alloc = effects.iter().any(|effect| effect == "alloc");
        // Capability classification is a direct-dependency heuristic: it tags the
        // function that directly calls the platform externs (the std.io wrappers),
        // keyed on those link names plus the declared effect. The build plan then
        // aggregates capabilities across the whole reachable set. A function that
        // reads/writes an already-open fd (no `open`/`creat` among its direct
        // deps) is intentionally not tagged read_file/write_file.
        let capability_name = if has_io
            && dependency_link_names.contains("open")
            && dependency_link_names.contains("read")
        {
            Some("read_file")
        } else if has_io
            && dependency_link_names.contains("creat")
            && dependency_link_names.contains("write")
        {
            Some("write_file")
        } else if has_io && dependency_link_names.contains("write") {
            Some("stdout")
        } else if has_alloc
            && (dependency_link_names.contains("malloc") || dependency_link_names.contains("free"))
        {
            Some("alloc")
        } else {
            None
        };
        let Some(name) = capability_name else {
            return Ok(());
        };
        out.insert(
            format!("{name}:{symbol_hash}"),
            PlannedCapability {
                name: name.to_string(),
                source,
                symbol_hash: symbol_hash.to_string(),
                effects: effects.to_vec(),
            },
        );
        Ok(())
    }

    fn external_link_names_for_direct_dependencies(
        &self,
        root: &ProgramRootPayload,
        definition_hash: &str,
    ) -> Result<BTreeSet<String>> {
        let mut link_names = BTreeSet::new();
        for dependency in self.dependencies_for_definition(root, definition_hash)? {
            let Some(entry) = self.root_symbol(root, &dependency) else {
                continue;
            };
            if self.definition_is_external(&entry.definition)? {
                link_names.insert(
                    self.external_function_metadata(&entry.definition)?
                        .link_name,
                );
            }
        }
        Ok(link_names)
    }

    fn ensure_planned_artifact_jobs(&mut self, planned: &PlannedLink) -> Result<()> {
        for object in &planned.objects {
            let cache_exists = self.lookup_cache(&object.object_key_input)?.is_some();
            self.ensure_artifact_job_for_cache_state(
                &object.object_cache_key,
                ArtifactKind::ObjectFile,
                cache_exists,
            )?;
        }
        let cache_exists = self.lookup_cache(&planned.link_plan_key_input)?.is_some();
        self.ensure_artifact_job_for_cache_state(
            &planned.link_plan_cache_key,
            ArtifactKind::LinkPlan,
            cache_exists,
        )?;
        Ok(())
    }

    fn abi_symbol_for_entry(&self, entry: &crate::model::RootSymbolPayload) -> Result<String> {
        if self.definition_is_external(&entry.definition)? {
            Ok(self
                .external_function_metadata(&entry.definition)?
                .link_name)
        } else {
            internal_abi_symbol(&entry.symbol)
        }
    }

    pub(crate) fn reachable_symbols(
        &self,
        root_hash: &str,
        entry_symbol: &str,
    ) -> Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        let mut ordered = Vec::new();
        self.visit_reachable_symbol(root_hash, entry_symbol, &mut seen, &mut ordered)?;
        Ok(ordered)
    }

    fn visit_reachable_symbol(
        &self,
        root_hash: &str,
        symbol: &str,
        seen: &mut BTreeSet<String>,
        ordered: &mut Vec<String>,
    ) -> Result<()> {
        if !seen.insert(symbol.to_string()) {
            return Ok(());
        }
        for dep in self.dependencies_for_symbol(root_hash, symbol)? {
            self.visit_reachable_symbol(root_hash, &dep, seen, ordered)?;
        }
        ordered.push(symbol.to_string());
        Ok(())
    }

    fn ensure_executable_entry(&self, prepared: &PreparedLink) -> Result<()> {
        let entry = prepared
            .plan
            .get("entry_symbol_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("link plan missing entry symbol"))?;
        let root = self.load_root(&prepared.root_hash)?;
        let root_entry = self
            .root_symbol(&root, entry)
            .ok_or_else(|| anyhow!("entry symbol missing from root {entry}"))?;
        let (params, return_type) = self.signature_parts(&root_entry.signature)?;
        if !params.is_empty() {
            bail!("native executable entry must not take parameters");
        }
        if crate::types::scalar_int_type_by_hash(&return_type).is_none()
            && return_type != type_hash_for("Bool")
        {
            bail!("native executable entry must return a sized integer or bool");
        }
        Ok(())
    }

    fn native_test_harness_source(
        &self,
        root: &ProgramRootPayload,
        prepared: &PreparedLink,
        entry_symbol: &str,
        expected: &Value,
        target_triple: &str,
    ) -> Result<String> {
        let root_entry = self
            .root_symbol(root, entry_symbol)
            .ok_or_else(|| anyhow!("entry symbol missing from root {entry_symbol}"))?;
        let (params, return_type_hash) = self.signature_parts(&root_entry.signature)?;
        if !params.is_empty() {
            bail!("native aggregate harness entry must not take parameters");
        }
        if !matches!(
            expected,
            Value::Array(_) | Value::Record(_) | Value::Enum { .. }
        ) {
            bail!("native aggregate harness requires an array, record, or enum expected value");
        }
        if !matches!(
            self.type_spec_in_root(root, &return_type_hash)?,
            TypeSpec::FixedArray { .. } | TypeSpec::Record(_) | TypeSpec::Enum(_)
        ) {
            bail!("native aggregate harness entry must return an array, record, or enum");
        }

        let layout = self
            .compute_type_layout(root, &return_type_hash, target_triple)?
            .metadata;
        if !matches!(
            layout.get("kind").and_then(JsonValue::as_str),
            Some("fixed_array" | "record" | "enum")
        ) {
            bail!("native aggregate harness expected array, record, or enum layout metadata");
        }
        let size_bytes = required_metadata_u64(&layout, "size_bytes")?;
        let align_bytes = required_metadata_u64(&layout, "align_bytes")?;
        let storage_bytes = size_bytes.max(1);
        let entry_abi_symbol = prepared
            .plan
            .get("entry_abi_symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("link plan missing entry ABI symbol"))?;
        let mut comparisons = String::new();
        let mut next_check = 1;
        self.native_value_comparison(
            root,
            &return_type_hash,
            expected,
            0,
            target_triple,
            &mut next_check,
            &mut comparisons,
        )?;
        let export_wrappers = export_wrapper_source(&prepared.plan)?;
        let return_abi = layout
            .get("abi")
            .and_then(|abi| abi.get("return"))
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("native record layout missing return ABI"))?;
        let call = match return_abi {
            "hidden_return_slot" => format!(
                "{ARGV_RUNTIME_SOURCE}void *{entry_abi_symbol}(void *out);\nstruct codedb_result {{ unsigned char bytes[{storage_bytes}]; }} __attribute__((aligned({align_bytes})));\nint main(void) {{\n  struct codedb_result out;\n  memset(&out, 0, sizeof(out));\n  {entry_abi_symbol}(&out);\n"
            ),
            "by_value" => format!(
                "{ARGV_RUNTIME_SOURCE}uint64_t {entry_abi_symbol}(void);\nstruct codedb_result {{ unsigned char bytes[{storage_bytes}]; }} __attribute__((aligned({align_bytes})));\nint main(void) {{\n  struct codedb_result out;\n  memset(&out, 0, sizeof(out));\n  uint64_t result = {entry_abi_symbol}();\n  memcpy(&out, &result, {size_bytes});\n"
            ),
            other => bail!("native aggregate harness unsupported return ABI {other}"),
        };
        Ok(format!(
            "{export_wrappers}#include <stdint.h>\n#include <string.h>\n{call}{comparisons}  return 0;\n}}\n"
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn native_record_comparisons(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        expected: &Value,
        layout: &JsonValue,
        base_offset: u64,
        target_triple: &str,
        next_check: &mut u32,
        out: &mut String,
    ) -> Result<()> {
        let Value::Record(values) = expected else {
            bail!("native record comparison expected record value");
        };
        let TypeSpec::Record(fields) = self.type_spec_in_root(root, type_hash)? else {
            bail!("native record comparison expected record type");
        };
        if values.len() != fields.len() {
            bail!(
                "native record comparison expected {} fields, got {}",
                fields.len(),
                values.len()
            );
        }
        let field_layouts = native_record_field_layouts(layout)?;
        for field in fields {
            let value = values
                .get(&field.name)
                .ok_or_else(|| anyhow!("native record comparison missing field {}", field.name))?;
            let field_layout = field_layouts
                .get(&field.name)
                .ok_or_else(|| anyhow!("native record layout missing field {}", field.name))?;
            if field_layout.type_hash != field.type_hash {
                bail!("native record layout field {} type mismatch", field.name);
            }
            self.native_value_comparison(
                root,
                &field.type_hash,
                &value.borrow(),
                base_offset + field_layout.offset_bytes,
                target_triple,
                next_check,
                out,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn native_value_comparison(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        expected: &Value,
        offset: u64,
        target_triple: &str,
        next_check: &mut u32,
        out: &mut String,
    ) -> Result<()> {
        match (expected, self.type_spec_in_root(root, type_hash)?) {
            (Value::I64(value), TypeSpec::Builtin(kind)) if kind == "I64" => {
                let code = next_native_check_code(next_check);
                out.push_str(&format!(
                    "  {{ int64_t actual; memcpy(&actual, ((const unsigned char *)&out) + {offset}, sizeof(actual)); if (actual != {}) return {code}; }}\n",
                    c_i64_literal(*value)
                ));
                Ok(())
            }
            (Value::Bool(value), TypeSpec::Builtin(kind)) if kind == "Bool" => {
                let code = next_native_check_code(next_check);
                let expected = u8::from(*value);
                out.push_str(&format!(
                    "  {{ uint8_t actual; memcpy(&actual, ((const unsigned char *)&out) + {offset}, sizeof(actual)); if (actual != {expected}) return {code}; }}\n"
                ));
                Ok(())
            }
            (Value::Unit, TypeSpec::Builtin(kind)) if kind == "Unit" => Ok(()),
            (Value::Array(values), TypeSpec::FixedArray { element, len }) => {
                if values.len() as u64 != len {
                    bail!(
                        "native array comparison expected {} elements, got {}",
                        len,
                        values.len()
                    );
                }
                let layout = self
                    .compute_type_layout(root, type_hash, target_triple)?
                    .metadata;
                if layout.get("kind").and_then(JsonValue::as_str) != Some("fixed_array") {
                    bail!("native array comparison expected fixed_array layout");
                }
                if layout.get("element_type_hash").and_then(JsonValue::as_str)
                    != Some(element.as_str())
                    || layout.get("len").and_then(JsonValue::as_u64) != Some(len)
                {
                    bail!("native array layout metadata mismatch");
                }
                let stride_bytes = required_metadata_u64(&layout, "stride_bytes")?;
                for (idx, value) in values.iter().enumerate() {
                    self.native_value_comparison(
                        root,
                        &element,
                        &value.borrow(),
                        offset + stride_bytes * idx as u64,
                        target_triple,
                        next_check,
                        out,
                    )?;
                }
                Ok(())
            }
            (Value::Record(_), TypeSpec::Record(_)) => {
                let layout = self
                    .compute_type_layout(root, type_hash, target_triple)?
                    .metadata;
                self.native_record_comparisons(
                    root,
                    type_hash,
                    expected,
                    &layout,
                    offset,
                    target_triple,
                    next_check,
                    out,
                )
            }
            (Value::Enum { variant, value }, TypeSpec::Enum(variants)) => {
                let layout = self
                    .compute_type_layout(root, type_hash, target_triple)?
                    .metadata;
                let variant_layouts = native_enum_variant_layouts(&layout)?;
                let variant_layout = variant_layouts
                    .get(variant)
                    .ok_or_else(|| anyhow!("native enum layout missing variant {variant}"))?;
                let variant_type = variants
                    .iter()
                    .find(|candidate| candidate.name == *variant)
                    .ok_or_else(|| anyhow!("native enum comparison unknown variant {variant}"))?;
                if variant_layout.type_hash != variant_type.type_hash {
                    bail!("native enum layout variant {variant} type mismatch");
                }
                let tag_code = next_native_check_code(next_check);
                out.push_str(&format!(
                    "  {{ uint64_t actual; memcpy(&actual, ((const unsigned char *)&out) + {offset}, sizeof(actual)); if (actual != {}ULL) return {tag_code}; }}\n",
                    variant_layout.tag_value
                ));
                self.native_value_comparison(
                    root,
                    &variant_type.type_hash,
                    &value.borrow(),
                    offset + variant_layout.payload_offset_bytes,
                    target_triple,
                    next_check,
                    out,
                )
            }
            _ => bail!(
                "native aggregate harness direct comparison supports only semantic test values containing i64, bool, unit, nested arrays, nested records, or nested enums; reference-carrying aggregates must be tested through native-required scalar entrypoints"
            ),
        }
    }
}

pub(crate) fn native_process_entry_metadata(
    entry_symbol_hash: &str,
    entry_abi_symbol: &str,
    param_type_hashes: &[String],
    return_type_hash: &str,
    effects: &[String],
) -> JsonValue {
    json!({
        "schema": ENTRY_POINT_METADATA_SCHEMA,
        "kind": "process",
        "entry_symbol_hash": entry_symbol_hash,
        "entry_abi_symbol": entry_abi_symbol,
        "signature": {
            "param_type_hashes": param_type_hashes,
            "return_type_hash": return_type_hash,
            "effects": effects,
        },
        "args": {
            "supported": true,
            "source": "process-argv",
            "runtime": "cc-harness-argv-capture",
            "capability_source": "build_plan.capabilities",
        },
        "stdout": {
            "supported": effects.iter().any(|effect| effect == "io"),
            "capability_source": "build_plan.capabilities",
        },
        "exit_code": {
            "source": "entry_return_value",
            "harness": "c-main-return-entry-value",
            "success_code": 0,
        },
        "runtime": {
            "semantic_interpreter": false,
            "dispatcher": false,
            "linker": "cc",
        },
    })
}

fn prepared_object(object: NativeObjectArtifact) -> PreparedObject {
    PreparedObject {
        artifact_hash: object.artifact_hash,
        cache_key: object.cache_key,
        bytes: object.bytes,
    }
}

fn required_metadata_value(metadata: &JsonValue, key: &str) -> Result<JsonValue> {
    metadata
        .get(key)
        .cloned()
        .ok_or_else(|| anyhow!("native object metadata missing {key}"))
}

fn required_metadata_str<'a>(metadata: &'a JsonValue, key: &str) -> Result<&'a str> {
    metadata
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("native object metadata missing string {key}"))
}

fn required_metadata_u64(metadata: &JsonValue, key: &str) -> Result<u64> {
    metadata
        .get(key)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| anyhow!("native metadata missing u64 {key}"))
}

fn platform_external_symbols_from_objects(objects: &[JsonValue]) -> Result<Vec<JsonValue>> {
    let mut symbols = BTreeMap::new();
    for object in objects {
        for relocation in object
            .get("relocations")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("link object missing relocations"))?
        {
            if relocation.get("platform").and_then(JsonValue::as_bool) != Some(true) {
                continue;
            }
            let symbol_hash = relocation
                .get("target_symbol_hash")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("platform relocation missing target_symbol_hash"))?;
            let abi_symbol = relocation
                .get("target_abi_symbol")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("platform relocation missing target_abi_symbol"))?;
            symbols.insert(
                symbol_hash.to_string(),
                json!({
                    "symbol_hash": symbol_hash,
                    "link_name": abi_symbol,
                    "platform": true,
                }),
            );
        }
    }
    Ok(symbols.into_values().collect())
}

fn merge_platform_external_symbol_entries(
    mut first: Vec<JsonValue>,
    second: Vec<JsonValue>,
) -> Vec<JsonValue> {
    let mut symbols = BTreeMap::new();
    for entry in first.drain(..).chain(second) {
        let key = entry
            .get("symbol_hash")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string();
        symbols.insert(key, entry);
    }
    symbols.into_values().collect()
}

fn collect_semantic_platform_external(
    root: &ProgramRootPayload,
    symbol_hash: &str,
    link_name: &str,
    out: &mut BTreeMap<String, PlannedPlatformExternalSymbol>,
) -> Result<()> {
    let Some(source) = qualified_symbol_display(root, symbol_hash) else {
        return Ok(());
    };
    // Recognize a platform-capsule extern by its nature — an external (FFI)
    // declaration (this is only called under `definition_is_external`) whose
    // link name is a minimal platform extern — NOT by the module it happens to
    // live in. Keying on a literal `std.platform.` prefix made the build-plan
    // metadata fragile: a `move_symbol` out of that module silently dropped the
    // extern from the reported capsule (and verify, sharing the rule, could not
    // catch it). The link name is rename/move-stable.
    if !is_minimal_platform_extern(link_name) {
        return Ok(());
    }
    out.insert(
        symbol_hash.to_string(),
        PlannedPlatformExternalSymbol {
            symbol_hash: symbol_hash.to_string(),
            link_name: link_name.to_string(),
            source,
        },
    );
    Ok(())
}

fn collect_compiler_platform_externals(
    ir: &LoweredFunctionIr,
    out: &mut BTreeMap<String, PlannedPlatformExternalSymbol>,
) {
    let usage = compiler_platform_usage_for_ir(ir);
    if usage.uses_malloc {
        insert_compiler_platform_external(out, "platform:malloc", "malloc", "compiler.heap_alloc");
    }
    if usage.uses_free {
        insert_compiler_platform_external(out, "platform:free", "free", "compiler.drop");
    }
    if usage.uses_args {
        // Defined by the cc link harness (ARGV_RUNTIME_SOURCE), not libc.
        insert_compiler_platform_external(
            out,
            "platform:arg_count",
            "codedb_arg_count",
            "compiler.process_args",
        );
        insert_compiler_platform_external(
            out,
            "platform:arg_len",
            "codedb_arg_len",
            "compiler.process_args",
        );
        insert_compiler_platform_external(
            out,
            "platform:arg_byte",
            "codedb_arg_byte",
            "compiler.process_args",
        );
    }
}

fn insert_compiler_platform_external(
    out: &mut BTreeMap<String, PlannedPlatformExternalSymbol>,
    symbol_hash: &str,
    link_name: &str,
    source: &str,
) {
    out.insert(
        symbol_hash.to_string(),
        PlannedPlatformExternalSymbol {
            symbol_hash: symbol_hash.to_string(),
            link_name: link_name.to_string(),
            source: source.to_string(),
        },
    );
}

#[derive(Debug, Clone, Copy, Default)]
struct CompilerPlatformUsage {
    uses_malloc: bool,
    uses_free: bool,
    uses_args: bool,
}

fn compiler_platform_usage_for_ir(ir: &LoweredFunctionIr) -> CompilerPlatformUsage {
    let mut usage = CompilerPlatformUsage::default();
    collect_compiler_platform_usage_from_ops(ir, &ir.operations, &mut usage);
    usage
}

fn collect_compiler_platform_usage_from_ops(
    ir: &LoweredFunctionIr,
    ops: &[LoweredOp],
    usage: &mut CompilerPlatformUsage,
) {
    for op in ops {
        match op {
            LoweredOp::HeapAlloc { .. }
            | LoweredOp::VecNew { .. }
            | LoweredOp::StringNew { .. } => {
                usage.uses_malloc = true;
            }
            LoweredOp::ArgCount { .. } | LoweredOp::ArgLen { .. } | LoweredOp::ArgByte { .. } => {
                usage.uses_args = true;
            }
            LoweredOp::Drop { type_hash, .. } => {
                if lowered_layout_contains_owned_resource(ir, type_hash) {
                    usage.uses_free = true;
                }
            }
            // Freeing a box shell always calls the platform `free` (SPEC_V3 §7).
            LoweredOp::FreeBoxShell { .. } => {
                usage.uses_free = true;
            }
            LoweredOp::If {
                then_block,
                else_block,
                ..
            } => {
                collect_compiler_platform_usage_from_ops(ir, &then_block.operations, usage);
                collect_compiler_platform_usage_from_ops(ir, &else_block.operations, usage);
            }
            LoweredOp::Case { arms, .. } => {
                for arm in arms {
                    collect_compiler_platform_usage_from_ops(ir, &arm.block.operations, usage);
                }
            }
            LoweredOp::Fold { body, .. } => {
                collect_compiler_platform_usage_from_ops(ir, &body.operations, usage);
            }
            _ => {}
        }
    }
}

fn lowered_layout_contains_owned_resource(ir: &LoweredFunctionIr, type_hash: &str) -> bool {
    ir.type_layouts
        .iter()
        .find(|layout| layout.type_hash == type_hash)
        .and_then(|layout| {
            layout
                .metadata
                .get("contains_owned_resource")
                .and_then(JsonValue::as_bool)
        })
        .unwrap_or(false)
}

fn is_minimal_platform_extern(link_name: &str) -> bool {
    matches!(
        link_name,
        "write" | "read" | "open" | "creat" | "close" | "malloc" | "free" | "trap" | "exit"
    )
}

fn native_record_field_layouts(
    layout: &JsonValue,
) -> Result<BTreeMap<String, NativeRecordFieldLayout>> {
    if layout.get("kind").and_then(JsonValue::as_str) != Some("record") {
        bail!("native record layout metadata must have kind record");
    }
    let mut fields = BTreeMap::new();
    for field in layout
        .get("fields")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| anyhow!("native record layout missing fields"))?
    {
        let name = field
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("native record layout field missing name"))?
            .to_string();
        let type_hash = field
            .get("type_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("native record layout field missing type_hash"))?
            .to_string();
        let offset_bytes = required_metadata_u64(field, "offset_bytes")?;
        if fields
            .insert(
                name.clone(),
                NativeRecordFieldLayout {
                    type_hash,
                    offset_bytes,
                },
            )
            .is_some()
        {
            bail!("native record layout has duplicate field {name}");
        }
    }
    Ok(fields)
}

fn native_enum_variant_layouts(
    layout: &JsonValue,
) -> Result<BTreeMap<String, NativeEnumVariantLayout>> {
    if layout.get("kind").and_then(JsonValue::as_str) != Some("enum") {
        bail!("native enum layout metadata must have kind enum");
    }
    let mut variants = BTreeMap::new();
    for variant in layout
        .get("variants")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| anyhow!("native enum layout missing variants"))?
    {
        let name = variant
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("native enum layout variant missing name"))?
            .to_string();
        let type_hash = variant
            .get("type_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("native enum layout variant missing type_hash"))?
            .to_string();
        let tag_value = required_metadata_u64(variant, "tag_value")?;
        let payload_offset_bytes = required_metadata_u64(variant, "payload_offset_bytes")?;
        if variants
            .insert(
                name.clone(),
                NativeEnumVariantLayout {
                    type_hash,
                    tag_value,
                    payload_offset_bytes,
                },
            )
            .is_some()
        {
            bail!("native enum layout has duplicate variant {name}");
        }
    }
    Ok(variants)
}

fn json_metadata(artifact_json: &JsonValue) -> Result<JsonValue> {
    if artifact_json.get("schema").and_then(JsonValue::as_str) == Some(LINK_PLAN_SCHEMA) {
        return Ok(artifact_json.clone());
    }
    artifact_json
        .get("metadata")
        .cloned()
        .ok_or_else(|| anyhow!("cached link plan missing metadata"))
}

fn executable_cache_key(prepared: &PreparedLink, linker_identity_hash: &str) -> CacheKeyInput {
    CacheKeyInput::new(
        ArtifactKind::Executable,
        &prepared.input_hash,
        EXTERNAL_CC_LINKER_BACKEND_ID,
        prepared.plan["target_triple"]
            .as_str()
            .unwrap_or(DEFAULT_NATIVE_TARGET),
    )
    .with_dependency_implementation_hashes(
        prepared
            .objects
            .iter()
            .map(|object| object.cache_key.clone())
            .chain(std::iter::once(prepared.plan_hash.clone()))
            .chain(std::iter::once(linker_identity_hash.to_string()))
            .collect(),
    )
}

/// The argv runtime every cc harness defines (R12): the process arguments
/// (program name excluded) captured at startup, plus the accessors the
/// `arg_count`/`arg_len`/`arg_byte` lowered ops call (the malloc/free
/// platform-symbol pattern, except these are defined here rather than libc).
/// Out-of-range indices abort — the native form of the evaluator's range
/// error. Harnesses that never receive arguments (the test runners) keep the
/// zero defaults, matching an un-seeded evaluation.
const ARGV_RUNTIME_SOURCE: &str = "static long long codedb_args_count = 0;\n\
static char **codedb_args_values = 0;\n\
long long codedb_arg_count(void) { return codedb_args_count; }\n\
long long codedb_arg_len(long long i) {\n\
  if (i < 0 || i >= codedb_args_count) __builtin_trap();\n\
  long long n = 0;\n\
  while (codedb_args_values[i][n]) n++;\n\
  return n;\n\
}\n\
long long codedb_arg_byte(long long i, long long j) {\n\
  if (i < 0 || i >= codedb_args_count) __builtin_trap();\n\
  long long n = 0;\n\
  while (codedb_args_values[i][n]) n++;\n\
  if (j < 0 || j >= n) __builtin_trap();\n\
  return (long long)(unsigned char)codedb_args_values[i][j];\n\
}\n";

fn link_with_cc(prepared: &PreparedLink) -> Result<Vec<u8>> {
    let entry = prepared
        .plan
        .get("entry_abi_symbol")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("link plan missing entry ABI symbol"))?;
    let export_wrappers = export_wrapper_source(&prepared.plan)?;
    let harness_source = format!(
        "{export_wrappers}{ARGV_RUNTIME_SOURCE}long {entry}(void);\nint main(int argc, char **argv) {{\n  codedb_args_count = argc > 0 ? argc - 1 : 0;\n  codedb_args_values = argv + 1;\n  return (int){entry}();\n}}\n"
    );
    link_with_cc_harness(prepared, &harness_source)
}

fn link_with_cc_harness(prepared: &PreparedLink, harness_source: &str) -> Result<Vec<u8>> {
    let temp_dir = build_temp_dir(&prepared.plan_hash)?;
    std::fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;
    let mut object_paths = Vec::new();
    for (idx, object) in prepared.objects.iter().enumerate() {
        let path = temp_dir.join(format!("{idx}.o"));
        std::fs::write(&path, &object.bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
        object_paths.push(path);
    }
    let harness = temp_dir.join("codedb_main.c");
    std::fs::write(&harness, harness_source)
        .with_context(|| format!("failed to write {}", harness.display()))?;
    let executable = temp_dir.join("codedb_executable");
    let mut command = Command::new("cc");
    for object in &object_paths {
        command.arg(object);
    }
    for library in external_libraries(&prepared.plan)? {
        if library != "c" {
            command.arg(format!("-l{library}"));
        }
    }
    let output = command
        .arg(&harness)
        .arg("-o")
        .arg(&executable)
        .output()
        .context("failed to invoke cc linker")?;
    if !output.status.success() {
        bail!(
            "cc linker failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let bytes = std::fs::read(&executable)
        .with_context(|| format!("failed to read {}", executable.display()))?;
    let _ = std::fs::remove_dir_all(&temp_dir);
    Ok(bytes)
}

fn build_temp_dir(plan_hash: &str) -> Result<PathBuf> {
    let digest = plan_hash
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow!("plan hash must use sha256: prefix"))?;
    Ok(std::env::temp_dir().join(format!(
        "codedb-build-{}-{}",
        std::process::id(),
        &digest[..16]
    )))
}

fn host_linker_identity_for_target(target_triple: &str) -> Result<String> {
    let supported = match target_triple {
        APPLE_ARM64_TARGET => cfg!(all(target_os = "macos", target_arch = "aarch64")),
        LINUX_X86_64_TARGET => cfg!(all(target_os = "linux", target_arch = "x86_64")),
        _ => false,
    };
    if !supported {
        bail!(
            "cannot build executable for {target_triple} on this host with the external cc linker"
        );
    }
    let output = Command::new("cc")
        .arg("--version")
        .output()
        .context("cannot build executable: cc linker is not available")?;
    if !output.status.success() {
        bail!(
            "cannot identify cc linker\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(format!(
        "{EXTERNAL_CC_LINKER_BACKEND_ID}\0{target_triple}\0{}\0{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn export_wrapper_source(plan: &JsonValue) -> Result<String> {
    let mut out = String::new();
    let linked_internal_symbols = plan
        .get("objects")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(|object| {
            object
                .get("internal_abi_symbol")
                .and_then(JsonValue::as_str)
        })
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    for export in plan
        .get("export_map")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        let symbol = export
            .get("symbol_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("link plan export missing symbol_hash"))?;
        let internal = export
            .get("internal_abi_symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("link plan export missing internal_abi_symbol"))?;
        let exported = export
            .get("exported_abi_symbol")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("link plan export missing exported_abi_symbol"))?;
        validate_exported_abi_name(exported)?;
        if exported != internal && linked_internal_symbols.contains(exported) {
            bail!("exported ABI symbol {exported} conflicts with a linked internal ABI symbol");
        }
        if exported == internal {
            continue;
        }
        let object = plan_object_for_symbol(plan, symbol)?;
        let params = object
            .get("param_type_hashes")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("link plan object missing param_type_hashes"))?;
        let return_type = object
            .get("return_type_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("link plan object missing return_type_hash"))?;
        let return_c_type = native_harness_c_type(return_type)?;
        let params = params
            .iter()
            .enumerate()
            .map(|(idx, value)| {
                let ty = value
                    .as_str()
                    .ok_or_else(|| anyhow!("link plan param type must be a hash"))?;
                Ok(format!("{} a{idx}", native_harness_c_type(ty)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let declaration_params = if params.is_empty() {
            "void".to_string()
        } else {
            params.join(", ")
        };
        let call_args = (0..params.len())
            .map(|idx| format!("a{idx}"))
            .collect::<Vec<_>>()
            .join(", ");
        if return_c_type == "void" {
            out.push_str(&format!(
                "{return_c_type} {internal}({declaration_params});\n{return_c_type} {exported}({declaration_params}) {{ {internal}({call_args}); }}\n"
            ));
        } else {
            out.push_str(&format!(
                "{return_c_type} {internal}({declaration_params});\n{return_c_type} {exported}({declaration_params}) {{ return {internal}({call_args}); }}\n"
            ));
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

fn external_libraries(plan: &JsonValue) -> Result<Vec<String>> {
    let mut libraries = BTreeSet::new();
    for external in plan
        .get("external_symbols")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(library) = external.get("library").and_then(JsonValue::as_str) {
            libraries.insert(library.to_string());
        }
    }
    Ok(libraries.into_iter().collect())
}

fn plan_object_for_symbol<'a>(plan: &'a JsonValue, symbol: &str) -> Result<&'a JsonValue> {
    plan.get("objects")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .find(|object| object.get("symbol_hash").and_then(JsonValue::as_str) == Some(symbol))
        .ok_or_else(|| anyhow!("link plan export references unlinked symbol {symbol}"))
}

fn native_harness_c_type(type_hash: &str) -> Result<&'static str> {
    if type_hash == type_hash_for("I64") || type_hash == type_hash_for("Bool") {
        Ok("long")
    } else if type_hash == type_hash_for("U8") {
        Ok("unsigned char")
    } else if type_hash == type_hash_for("Unit") {
        Ok("void")
    } else {
        bail!("unsupported native harness type {type_hash}")
    }
}

fn next_native_check_code(next_check: &mut u32) -> u32 {
    let code = ((*next_check - 1) % 250) + 1;
    *next_check += 1;
    code
}

fn c_i64_literal(value: i64) -> String {
    if value == i64::MIN {
        "(-9223372036854775807LL - 1LL)".to_string()
    } else {
        format!("{value}LL")
    }
}

fn link_options(target_triple: &str) -> Result<JsonValue> {
    match target_triple {
        LINUX_X86_64_TARGET | APPLE_ARM64_TARGET => Ok(json!({
            "linker": "cc",
            "entry_harness": "c-main-return-entry-value",
        })),
        other => bail!("unsupported native link target {other}"),
    }
}

impl CodeDb {
    /// Per-function native stack-frame report for the main-branch closure of `entry`
    /// (default `main`), targeting `target` (default the host arm64 target). The offline,
    /// per-function mirror of the v0 backend's frame-size and machine-parameter `bail!`s
    /// (Phase 15a.5.2): functions are listed largest-frame first, with near-limit and
    /// over-limit flags, so a frame or param-cap overflow is caught before the slow native
    /// build instead of one function at a time during it. Returns a text table, or pretty
    /// JSON when `json` is set. Frame sizes reuse the target's real `StackLayout`, so the
    /// report cannot drift from the backend; non-function and external symbols (no frame)
    /// are skipped.
    pub fn frame_report_main_branch(
        &mut self,
        entry: &str,
        target: Option<&str>,
        near: u32,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let target = target.unwrap_or(DEFAULT_NATIVE_TARGET);
        let (frame_limit, param_cap) = target_frame_limits(target)?;
        let branch = self.branch(MAIN_BRANCH)?;
        let root_hash = branch.root_hash.clone();
        let entry_symbol = self
            .resolve_symbol_or_name(&root_hash, entry)
            .map_err(|err| anyhow!("unknown entry function {entry}: {err}"))?;
        let root = self.load_root(&root_hash)?;
        let symbols = self.reachable_symbols(&root_hash, &entry_symbol)?;

        let mut reports = Vec::new();
        for symbol in symbols {
            let root_entry = self
                .root_symbol(&root, &symbol)
                .ok_or_else(|| anyhow!("frame report symbol missing from root {symbol}"))?
                .clone();
            if !self.definition_is_internal_function(&root_entry.definition)? {
                continue;
            }
            let ir = self.build_lowered_function_ir(&root, &root_entry, target)?;
            let stats = frame_stats(&ir, target)?;
            let name = qualified_symbol_display(&root, &symbol).unwrap_or_else(|| symbol.clone());
            reports.push((name, symbol, stats));
        }
        reports.sort_by(|a, b| {
            b.2.stack_size
                .cmp(&a.2.stack_size)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });

        Ok(if json {
            render_frame_report_json(entry, target, near, frame_limit, param_cap, &reports)
        } else {
            render_frame_report_text(entry, target, near, frame_limit, param_cap, &reports)
        })
    }
}

/// True when `stats` is within `near` bytes of (or over) the target frame limit.
fn frame_is_near_limit(stats: &FrameStats, near: u32) -> bool {
    stats
        .frame_limit
        .is_some_and(|limit| stats.stack_size.saturating_add(near) >= limit)
}

fn render_frame_report_json(
    entry: &str,
    target: &str,
    near: u32,
    frame_limit: Option<u32>,
    param_cap: usize,
    reports: &[(String, String, FrameStats)],
) -> String {
    let functions = reports
        .iter()
        .map(|(name, symbol, s)| {
            json!({
                "name": name,
                "symbol": symbol,
                "stack_size": s.stack_size,
                "near_limit": frame_is_near_limit(s, near),
                "over_frame_limit": s.over_frame_limit,
                "machine_params": s.machine_params,
                "machine_param_cap": s.machine_param_cap,
                "over_param_cap": s.over_param_cap,
                "hidden_return_slot": s.hidden_return_slot,
                "aggregate_local_count": s.aggregate_local_count,
                "aggregate_local_bytes": s.aggregate_local_bytes,
                "indirect_param_count": s.indirect_param_count,
                "indirect_param_bytes": s.indirect_param_bytes,
                "value_id_count": s.value_id_count,
            })
        })
        .collect::<Vec<_>>();
    let report = json!({
        "schema": "codedb/frame-report/v1",
        "entry": entry,
        "target": target,
        "frame_limit": frame_limit,
        "param_cap": param_cap,
        "near_threshold": near,
        "function_count": reports.len(),
        "max_stack_size": reports.iter().map(|(_, _, s)| s.stack_size).max().unwrap_or(0),
        "over_frame_limit_count": reports.iter().filter(|(_, _, s)| s.over_frame_limit).count(),
        "over_param_cap_count": reports.iter().filter(|(_, _, s)| s.over_param_cap).count(),
        "near_limit_count": reports
            .iter()
            .filter(|(_, _, s)| frame_is_near_limit(s, near))
            .count(),
        "functions": functions,
    });
    format!("{}\n", serde_json::to_string_pretty(&report).unwrap_or_default())
}

fn render_frame_report_text(
    entry: &str,
    target: &str,
    near: u32,
    frame_limit: Option<u32>,
    param_cap: usize,
    reports: &[(String, String, FrameStats)],
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let limit_str = frame_limit.map_or_else(|| "none".to_string(), |limit| limit.to_string());
    let max_frame = reports.iter().map(|(_, _, s)| s.stack_size).max().unwrap_or(0);
    let over_frame = reports.iter().filter(|(_, _, s)| s.over_frame_limit).count();
    let over_param = reports.iter().filter(|(_, _, s)| s.over_param_cap).count();
    let near_count = reports
        .iter()
        .filter(|(_, _, s)| frame_is_near_limit(s, near))
        .count();
    let _ = writeln!(
        out,
        "frame-report  entry={entry}  target={target}  frame_limit={limit_str}  param_cap={param_cap}  near={near}"
    );
    let _ = writeln!(
        out,
        "{} functions  max_frame={max_frame}  over_frame={over_frame}  over_param={over_param}  near_limit={near_count}",
        reports.len()
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {:>6}  {:>7}  {:>12}  {:>7}  {:<10}  FUNCTION",
        "FRAME", "PARAMS", "AGG_LOCALS", "VALUES", "FLAGS"
    );
    for (name, _symbol, s) in reports {
        let flag = if s.over_frame_limit {
            "OVER-FRAME"
        } else if s.over_param_cap {
            "OVER-PARAM"
        } else if frame_is_near_limit(s, near) {
            "NEAR"
        } else {
            "ok"
        };
        let params = format!("{}/{}", s.machine_params, s.machine_param_cap);
        let agg = format!("{}/{}", s.aggregate_local_count, s.aggregate_local_bytes);
        let _ = writeln!(
            out,
            "  {:>6}  {:>7}  {:>12}  {:>7}  {:<10}  {name}",
            s.stack_size, params, agg, s.value_id_count, flag
        );
    }
    out
}
