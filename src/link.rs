use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value as JsonValue, json};

use crate::abi::{export_map, internal_abi_symbol, validate_exported_abi_name};
use crate::artifact::CacheKeyInput;
use crate::backend::ArtifactKind;
use crate::backend::native::NativeObjectArtifact;
use crate::jobs::{ArtifactJobClaim, artifact_job_error, new_worker_id};
use crate::model::ProgramRootPayload;
use crate::store::{CodeDb, cache_key_for_input, canonical_json, hash_bytes};
use crate::types::type_hash_for;
use crate::{
    APPLE_ARM64_TARGET, BYTES_DOMAIN, DEFAULT_NATIVE_TARGET, LINUX_X86_64_TARGET, MAIN_BRANCH,
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

struct PreparedLink {
    input_hash: String,
    link_plan_cache_key: String,
    plan: JsonValue,
    plan_hash: String,
    objects: Vec<PreparedObject>,
}

struct PreparedObject {
    artifact_hash: String,
    cache_key: String,
    bytes: Vec<u8>,
}

impl PreparedLink {
    fn job_cache_keys(&self) -> Vec<String> {
        self.objects
            .iter()
            .map(|object| object.cache_key.clone())
            .chain(std::iter::once(self.link_plan_cache_key.clone()))
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
        let prepared = self.prepare_link_plan_main_branch(entry_name, target_triple)?;
        let jobs = self.artifact_job_json_for_cache_keys(&prepared.job_cache_keys())?;
        let payload = json!({
            "schema": "codedb/native-build-plan/v1",
            "target_triple": prepared.plan["target_triple"].clone(),
            "entry_symbol_hash": prepared.plan["entry_symbol_hash"].clone(),
            "entry_abi_symbol": prepared.plan["entry_abi_symbol"].clone(),
            "link_plan_input_hash": prepared.input_hash,
            "link_plan_cache_key": prepared.link_plan_cache_key,
            "link_plan_hash": prepared.plan_hash,
            "artifact_kinds": ["object_file", "link_plan", "executable"],
            "jobs": jobs,
            "objects": prepared.plan["objects"].clone(),
            "export_map": prepared.plan["export_map"].clone(),
            "external_symbols": prepared.plan["external_symbols"].clone(),
            "link_options": prepared.plan["link_options"].clone(),
        });
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn build_main_branch(
        &mut self,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<NativeBuild> {
        let prepared = self.prepare_link_plan_main_branch(entry_name, target_triple)?;
        self.ensure_executable_entry(&prepared.plan)?;

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

    fn prepare_link_plan_main_branch(
        &mut self,
        entry_name: &str,
        target_triple: &str,
    ) -> Result<PreparedLink> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let root = self.load_root(&branch.root_hash)?;
        let entry_symbol = self
            .resolve_name(&branch.root_hash, "main", entry_name)
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
        let symbols = self.reachable_symbols(root_hash, entry_symbol)?;
        let linked_symbols = symbols.iter().cloned().collect::<BTreeSet<_>>();
        let native_objects =
            self.emit_objects_for_symbols_parallel(root_hash, &symbols, target_triple)?;
        let mut objects = Vec::new();
        let mut object_entries = Vec::new();
        for (symbol, object) in symbols.into_iter().zip(native_objects.into_iter()) {
            let root_entry = self
                .root_symbol(root, &symbol)
                .ok_or_else(|| anyhow!("link plan symbol missing from root {symbol}"))?;
            let (param_type_hashes, return_type_hash) =
                self.signature_parts(&root_entry.signature)?;
            let object_metadata = object.metadata.clone();
            let internal_abi = internal_abi_symbol(&symbol)?;
            object_entries.push(json!({
                "symbol_hash": &symbol,
                "definition_hash": &root_entry.definition,
                "signature_hash": &root_entry.signature,
                "param_type_hashes": param_type_hashes,
                "return_type_hash": return_type_hash,
                "internal_abi_symbol": &internal_abi,
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
            }));
            objects.push(prepared_object(object));
        }

        let exports = export_map(root)?
            .into_iter()
            .filter(|export| linked_symbols.contains(&export.symbol))
            .map(|export| {
                json!({
                    "symbol_hash": export.symbol,
                    "internal_abi_symbol": export.internal_abi_symbol,
                    "exported_abi_symbol": export.exported_name,
                })
            })
            .collect::<Vec<_>>();
        let input = json!({
            "schema": LINK_INPUT_SCHEMA,
            "target_triple": target_triple,
            "entry_symbol_hash": entry_symbol,
            "entry_abi_symbol": internal_abi_symbol(entry_symbol)?,
            "object_artifact_hashes": objects
                .iter()
                .map(|object| object.artifact_hash.clone())
                .collect::<Vec<_>>(),
            "object_cache_keys": objects
                .iter()
                .map(|object| object.cache_key.clone())
                .collect::<Vec<_>>(),
            "export_map": exports,
            "output_kind": "executable",
            "link_options": link_options(target_triple)?,
        });
        let input_hash = self.put_object("LinkPlanInput", &input)?;
        let mut plan = json!({
            "schema": LINK_PLAN_SCHEMA,
            "input_hash": &input_hash,
            "target_triple": target_triple,
            "entry_symbol_hash": entry_symbol,
            "entry_abi_symbol": internal_abi_symbol(entry_symbol)?,
            "objects": object_entries,
            "export_map": input["export_map"].clone(),
            "external_symbols": [],
            "output_kind": input["output_kind"].clone(),
            "link_options": input["link_options"].clone(),
        });
        let object_cache_keys = objects
            .iter()
            .map(|object| object.cache_key.clone())
            .collect::<Vec<_>>();
        let key_input = CacheKeyInput::new(
            ArtifactKind::LinkPlan,
            &input_hash,
            LINK_PLAN_BACKEND_ID,
            target_triple,
        )
        .with_dependency_implementation_hashes(object_cache_keys);
        let plan_cache_key = cache_key_for_input(&key_input)?;
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
            input_hash,
            link_plan_cache_key: plan_cache_key,
            plan,
            plan_hash,
            objects,
        })
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

    fn ensure_executable_entry(&self, plan: &JsonValue) -> Result<()> {
        let entry = plan
            .get("entry_symbol_hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("link plan missing entry symbol"))?;
        let root_hash = self.branch(MAIN_BRANCH)?.root_hash;
        let root = self.load_root(&root_hash)?;
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
    let entry = prepared
        .plan
        .get("entry_abi_symbol")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("link plan missing entry ABI symbol"))?;
    let harness = temp_dir.join("codedb_main.c");
    let export_wrappers = export_wrapper_source(&prepared.plan)?;
    std::fs::write(
        &harness,
        format!(
            "{export_wrappers}long {entry}(void);\nint main(void) {{ return (int){entry}(); }}\n"
        ),
    )
    .with_context(|| format!("failed to write {}", harness.display()))?;
    let executable = temp_dir.join("codedb_executable");
    let mut command = Command::new("cc");
    for object in &object_paths {
        command.arg(object);
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

fn link_options(target_triple: &str) -> Result<JsonValue> {
    match target_triple {
        LINUX_X86_64_TARGET | APPLE_ARM64_TARGET => Ok(json!({
            "linker": "cc",
            "entry_harness": "c-main-return-entry-value",
        })),
        other => bail!("unsupported native link target {other}"),
    }
}
