use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value as JsonValue, json};

use crate::abi::{export_map, internal_abi_symbol, validate_exported_abi_name};
use crate::artifact::CacheKeyInput;
use crate::backend::ArtifactKind;
use crate::backend::native::{NativeObjectArtifact, backend_id_for_target};
use crate::expr::Value;
use crate::jobs::{ArtifactJobClaim, artifact_job_error, new_worker_id};
use crate::model::ProgramRootPayload;
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

struct PlannedLink {
    input: JsonValue,
    input_hash: String,
    link_plan_cache_key: String,
    link_plan_key_input: CacheKeyInput,
    target_triple: String,
    entry_symbol_hash: String,
    entry_abi_symbol: String,
    entry_effects: Vec<String>,
    objects: Vec<PlannedObject>,
    external_symbols: Vec<PlannedExternalSymbol>,
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
            "link_plan_input_hash": &planned.input_hash,
            "link_plan_cache_key": &planned.link_plan_cache_key,
            "link_plan_hash": link_plan_hash,
            "artifact_kinds": ["object_file", "link_plan", "executable"],
            "jobs": jobs,
            "objects": planned.object_job_entries(),
            "export_map": &planned.export_map,
            "external_symbols": planned.external_symbol_entries(),
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
            harness_kind: "c-main-compare-record-return".to_string(),
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
            }));
            objects.push(prepared_object(object));
        }

        let input_hash = self.put_object("LinkPlanInput", &planned.input)?;
        if input_hash != planned.input_hash {
            bail!("computed link input hash does not match planned link input hash");
        }
        let mut plan = json!({
            "schema": LINK_PLAN_SCHEMA,
            "input_hash": &input_hash,
            "target_triple": &planned.target_triple,
            "entry_symbol_hash": &planned.entry_symbol_hash,
            "entry_abi_symbol": &planned.entry_abi_symbol,
            "objects": object_entries,
            "export_map": planned.export_map.clone(),
            "external_symbols": planned.external_symbol_entries(),
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
        for symbol in symbols {
            let root_entry = self
                .root_symbol(root, &symbol)
                .ok_or_else(|| anyhow!("link plan symbol missing from root {symbol}"))?;
            let (param_type_hashes, return_type_hash) =
                self.signature_parts(&root_entry.signature)?;
            let effects = self.signature_effect_names(&root_entry.signature)?;
            if self.definition_is_external(&root_entry.definition)? {
                let external = self.external_function_metadata(&root_entry.definition)?;
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
        let entry = root
            .symbols
            .iter()
            .find(|entry| entry.symbol == entry_symbol)
            .ok_or_else(|| anyhow!("entry symbol missing from root {entry_symbol}"))?;
        let input = json!({
            "schema": LINK_INPUT_SCHEMA,
            "target_triple": target_triple,
            "entry_symbol_hash": entry_symbol,
            "entry_abi_symbol": self.abi_symbol_for_entry(entry)?,
            "object_cache_keys": &object_cache_keys,
            "external_symbols": &external_symbol_entries,
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
            entry_abi_symbol: self.abi_symbol_for_entry(entry)?,
            entry_effects: self.signature_effect_names(&entry.signature)?,
            objects,
            external_symbols,
            export_map,
            link_options,
        })
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
        if return_type != type_hash_for("I64") && return_type != type_hash_for("Bool") {
            bail!("native executable entry must return i64 or bool");
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
            bail!("native record harness entry must not take parameters");
        }
        if !matches!(expected, Value::Record(_)) {
            bail!("native record harness requires a record expected value");
        }
        if !matches!(
            self.type_spec_in_root(root, &return_type_hash)?,
            TypeSpec::Record(_)
        ) {
            bail!("native record harness entry must return a record");
        }

        let layout = self
            .compute_type_layout(root, &return_type_hash, target_triple)?
            .metadata;
        if layout.get("kind").and_then(JsonValue::as_str) != Some("record") {
            bail!("native record harness expected record layout metadata");
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
        self.native_record_comparisons(
            root,
            &return_type_hash,
            expected,
            &layout,
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
                "void *{entry_abi_symbol}(void *out);\nstruct codedb_result {{ unsigned char bytes[{storage_bytes}]; }} __attribute__((aligned({align_bytes})));\nint main(void) {{\n  struct codedb_result out;\n  memset(&out, 0, sizeof(out));\n  {entry_abi_symbol}(&out);\n"
            ),
            "by_value" => format!(
                "uint64_t {entry_abi_symbol}(void);\nstruct codedb_result {{ unsigned char bytes[{storage_bytes}]; }} __attribute__((aligned({align_bytes})));\nint main(void) {{\n  struct codedb_result out;\n  memset(&out, 0, sizeof(out));\n  uint64_t result = {entry_abi_symbol}();\n  memcpy(&out, &result, {size_bytes});\n"
            ),
            other => bail!("native record harness unsupported return ABI {other}"),
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
            _ => bail!(
                "native record harness supports only record values containing i64, bool, unit, or nested records"
            ),
        }
    }
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

fn link_with_cc(prepared: &PreparedLink) -> Result<Vec<u8>> {
    let entry = prepared
        .plan
        .get("entry_abi_symbol")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("link plan missing entry ABI symbol"))?;
    let export_wrappers = export_wrapper_source(&prepared.plan)?;
    let harness_source = format!(
        "{export_wrappers}long {entry}(void);\nint main(void) {{ return (int){entry}(); }}\n"
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
