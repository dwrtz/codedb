use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value as JsonValue, json};

use crate::abi::{
    export_map, internal_abi_symbol, validate_export_map, validate_exported_abi_name,
};
use crate::artifact::{ARTIFACT_METADATA_SCHEMA, CacheKeyInput};
use crate::backend::native::{
    ELF_BACKEND_ID, ElfObjectBackend, MACHO_BACKEND_ID, MachOArm64ObjectBackend,
};
use crate::backend::{ArtifactKind, ObjectBackend, ObjectBackendInput};
use crate::backend_c::ensure_no_forbidden_runtime_calls;
use crate::diff::dependency_pairs;
use crate::layout::{TYPE_LAYOUT_BACKEND_ID, TYPE_LAYOUT_SCHEMA, type_layout_cache_key_input};
use crate::lowering::{
    LoweredFunctionIr, LoweredOp, lowered_op_id_for_value, lowered_value_debug_ops,
};
use crate::migrations::{Operation, history_hash, migration_hash};
use crate::model::{
    ProgramRootPayload, ROOT_MODULES_METADATA_KEY, preferred_names, validate_module_path,
    validate_projection_identifier,
};
use crate::store::{
    CodeDb, cache_key_for_input, canonical_json, extract_hash_strings, function_interface_metadata,
    hash_bytes, hash_object_canonical,
};
use crate::types::{
    effect_names, member_defs_from_payload, region_params_from_payload, type_payload_for_spec,
    type_spec_from_payload, validate_external_abi_tag, validate_external_library_name,
    validate_external_link_name, validate_member_defs, validate_region_params,
};
use crate::{BYTES_DOMAIN, SCHEMA_VERSION};

impl CodeDb {
    pub fn verify(&mut self) -> Result<String> {
        self.prepare_verify()?;
        let mut errors = Vec::new();
        self.verify_objects(&mut errors)?;
        self.verify_edges(&mut errors)?;
        self.verify_branches(&mut errors)?;
        self.verify_migrations_and_histories(&mut errors)?;
        self.verify_roots(&mut errors)?;
        self.verify_caches(&mut errors)?;
        self.verify_workspace_transactions(&mut errors)?;
        self.verify_artifact_jobs(&mut errors)?;
        self.verify_history_replay_readonly(&mut errors)?;

        if errors.is_empty() {
            Ok("verify ok\n".to_string())
        } else {
            bail!("verify failed\n{}", errors.join("\n"));
        }
    }

    fn prepare_verify(&mut self) -> Result<()> {
        let object_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))?;
        let branch_count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM branches", [], |row| row.get(0))?;
        if object_count == 0 && branch_count == 0 {
            self.insert_builtin_types()?;
        }
        Ok(())
    }

    fn verify_objects(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT hash, kind, schema_version, payload_json FROM objects ORDER BY hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (hash, kind, schema_version, payload_json) = row?;
            match serde_json::from_str::<JsonValue>(&payload_json) {
                Ok(value) => {
                    let canonical = canonical_json(&value);
                    if canonical != payload_json {
                        errors.push(format!("corrupt_object: payload is not canonical {hash}"));
                    }
                    let recomputed = hash_object_canonical(&kind, schema_version, &canonical);
                    if recomputed != hash {
                        errors.push(format!("bad_hash: {hash} recomputes to {recomputed}"));
                    }
                    if kind == "Type" {
                        match type_spec_from_payload(&value) {
                            Ok(spec) => {
                                let expected = type_payload_for_spec(&spec)?;
                                if canonical_json(&expected) != canonical {
                                    errors.push(format!(
                                        "bad_type_object: {hash}: payload is not canonical"
                                    ));
                                }
                            }
                            Err(err) => errors.push(format!("bad_type_object: {hash}: {err:#}")),
                        }
                    }
                    self.verify_known_object_references(&hash, &kind, &value, errors)?;
                }
                Err(err) => errors.push(format!("corrupt_object: {hash}: {err}")),
            }
        }
        Ok(())
    }

    fn verify_known_object_references(
        &self,
        parent_hash: &str,
        kind: &str,
        payload: &JsonValue,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        match kind {
            "Type" => {
                self.verify_type_object_references(parent_hash, payload, errors)?;
            }
            "SymbolBirth" => {}
            "TypeDef" => {
                self.check_hash_ref(
                    parent_hash,
                    "type_symbol",
                    payload.get("type_symbol"),
                    errors,
                )?;
                self.check_hash_ref(parent_hash, "definition", payload.get("definition"), errors)?;
                match (
                    payload.get("type_kind").and_then(JsonValue::as_str),
                    payload.get("definition").and_then(JsonValue::as_str),
                ) {
                    (Some("record"), Some(definition)) => {
                        if self.get_kind(definition).ok().as_deref() != Some("RecordDef") {
                            errors.push(format!(
                                "bad_type_def: {parent_hash} record definition is not RecordDef"
                            ));
                        }
                    }
                    (Some("enum"), Some(definition)) => {
                        if self.get_kind(definition).ok().as_deref() != Some("EnumDef") {
                            errors.push(format!(
                                "bad_type_def: {parent_hash} enum definition is not EnumDef"
                            ));
                        }
                    }
                    (Some(other), _) => {
                        errors.push(format!("bad_type_def: {parent_hash} unknown kind {other}"))
                    }
                    (None, _) => errors.push(format!("bad_type_def: {parent_hash} missing kind")),
                }
            }
            "RecordDef" => {
                self.check_hash_ref(
                    parent_hash,
                    "type_symbol",
                    payload.get("type_symbol"),
                    errors,
                )?;
                self.verify_type_definition_payload(
                    parent_hash,
                    "record field",
                    "fields",
                    "field_symbol",
                    payload,
                    errors,
                )?;
            }
            "EnumDef" => {
                self.check_hash_ref(
                    parent_hash,
                    "type_symbol",
                    payload.get("type_symbol"),
                    errors,
                )?;
                self.verify_type_definition_payload(
                    parent_hash,
                    "enum variant",
                    "variants",
                    "variant_symbol",
                    payload,
                    errors,
                )?;
            }
            "FunctionSignature" => {
                if let Some(params) = payload.get("region_params").and_then(JsonValue::as_array) {
                    for (idx, param) in params.iter().enumerate() {
                        self.check_hash_ref(
                            parent_hash,
                            &format!("region_params[{idx}].region"),
                            param.get("region"),
                            errors,
                        )?;
                    }
                }
                self.check_hash_array_refs(parent_hash, "params", payload.get("params"), errors)?;
                self.check_hash_ref(parent_hash, "return", payload.get("return"), errors)?;
                self.check_signature_effects(parent_hash, payload.get("effects"), errors)?;
            }
            "Expression" => {
                self.check_hash_ref(parent_hash, "type", payload.get("type"), errors)?;
                match payload.get("expr_kind").and_then(JsonValue::as_str) {
                    Some(
                        "literal_i64" | "literal_bool" | "literal_unit" | "param_ref" | "local_ref",
                    ) => {}
                    Some("call") => {
                        self.check_hash_ref(parent_hash, "symbol", payload.get("symbol"), errors)?;
                        self.check_hash_array_refs(
                            parent_hash,
                            "args",
                            payload.get("args"),
                            errors,
                        )?;
                    }
                    Some("binary") => {
                        self.check_hash_ref(parent_hash, "left", payload.get("left"), errors)?;
                        self.check_hash_ref(parent_hash, "right", payload.get("right"), errors)?;
                    }
                    Some("unary") => {
                        self.check_hash_ref(parent_hash, "expr", payload.get("expr"), errors)?;
                    }
                    Some("borrow_shared" | "borrow_mut") => {
                        self.check_hash_ref(parent_hash, "target", payload.get("target"), errors)?;
                        self.check_hash_ref(parent_hash, "region", payload.get("region"), errors)?;
                        self.check_hash_ref(
                            parent_hash,
                            "referent_type",
                            payload.get("referent_type"),
                            errors,
                        )?;
                    }
                    Some("slice_from_array") => {
                        self.check_hash_ref(parent_hash, "target", payload.get("target"), errors)?;
                        self.check_hash_ref(
                            parent_hash,
                            "target_type",
                            payload.get("target_type"),
                            errors,
                        )?;
                        self.check_hash_ref(
                            parent_hash,
                            "array_type",
                            payload.get("array_type"),
                            errors,
                        )?;
                        self.check_hash_ref(
                            parent_hash,
                            "element_type",
                            payload.get("element_type"),
                            errors,
                        )?;
                        self.check_hash_ref(parent_hash, "region", payload.get("region"), errors)?;
                    }
                    Some("slice_len") => {
                        self.check_hash_ref(parent_hash, "target", payload.get("target"), errors)?;
                        self.check_hash_ref(
                            parent_hash,
                            "slice_type",
                            payload.get("slice_type"),
                            errors,
                        )?;
                    }
                    Some("subslice") => {
                        self.check_hash_ref(parent_hash, "target", payload.get("target"), errors)?;
                        self.check_hash_ref(parent_hash, "start", payload.get("start"), errors)?;
                        self.check_hash_ref(parent_hash, "len", payload.get("len"), errors)?;
                        self.check_hash_ref(
                            parent_hash,
                            "slice_type",
                            payload.get("slice_type"),
                            errors,
                        )?;
                        self.check_hash_ref(
                            parent_hash,
                            "element_type",
                            payload.get("element_type"),
                            errors,
                        )?;
                    }
                    Some("assign") => {
                        self.check_hash_ref(parent_hash, "target", payload.get("target"), errors)?;
                        self.check_hash_ref(parent_hash, "value", payload.get("value"), errors)?;
                        self.check_hash_ref(
                            parent_hash,
                            "target_type",
                            payload.get("target_type"),
                            errors,
                        )?;
                    }
                    Some("let") => {
                        self.check_hash_ref(
                            parent_hash,
                            "binding_type",
                            payload.get("binding_type"),
                            errors,
                        )?;
                        self.check_hash_ref(parent_hash, "value", payload.get("value"), errors)?;
                        self.check_hash_ref(parent_hash, "body", payload.get("body"), errors)?;
                    }
                    Some("if") => {
                        self.check_hash_ref(parent_hash, "cond", payload.get("cond"), errors)?;
                        self.check_hash_ref(parent_hash, "then", payload.get("then"), errors)?;
                        self.check_hash_ref(parent_hash, "else", payload.get("else"), errors)?;
                    }
                    Some("record_literal") => {
                        for (idx, field) in payload
                            .get("fields")
                            .and_then(JsonValue::as_array)
                            .into_iter()
                            .flatten()
                            .enumerate()
                        {
                            self.check_hash_ref(
                                parent_hash,
                                &format!("fields[{idx}].value"),
                                field.get("value"),
                                errors,
                            )?;
                            self.check_hash_ref(
                                parent_hash,
                                &format!("fields[{idx}].type"),
                                field.get("type"),
                                errors,
                            )?;
                        }
                    }
                    Some("field_access") => {
                        self.check_hash_ref(parent_hash, "target", payload.get("target"), errors)?;
                    }
                    Some("enum_construct") => {
                        self.check_hash_ref(
                            parent_hash,
                            "enum_type",
                            payload.get("enum_type"),
                            errors,
                        )?;
                        self.check_hash_ref(parent_hash, "value", payload.get("value"), errors)?;
                    }
                    Some("case") => {
                        self.check_hash_ref(parent_hash, "expr", payload.get("expr"), errors)?;
                        for (idx, arm) in payload
                            .get("arms")
                            .and_then(JsonValue::as_array)
                            .into_iter()
                            .flatten()
                            .enumerate()
                        {
                            self.check_hash_ref(
                                parent_hash,
                                &format!("arms[{idx}].body"),
                                arm.get("body"),
                                errors,
                            )?;
                        }
                    }
                    Some(_) | None => {}
                }
            }
            "FunctionDef" => {
                self.check_hash_ref(parent_hash, "symbol", payload.get("symbol"), errors)?;
                self.check_hash_ref(
                    parent_hash,
                    "function_sig_hash",
                    payload.get("function_sig_hash"),
                    errors,
                )?;
                self.check_hash_ref(
                    parent_hash,
                    "typed_body_expr_hash",
                    payload.get("typed_body_expr_hash"),
                    errors,
                )?;
            }
            "ExternalFunction" => {
                self.check_hash_ref(parent_hash, "symbol", payload.get("symbol"), errors)?;
                self.check_hash_ref(
                    parent_hash,
                    "function_sig_hash",
                    payload.get("function_sig_hash"),
                    errors,
                )?;
                match payload.get("abi").and_then(JsonValue::as_str) {
                    Some(abi) => {
                        if let Err(err) = validate_external_abi_tag(abi) {
                            errors.push(format!(
                                "bad_external_function: {parent_hash} invalid abi: {err:#}"
                            ));
                        }
                    }
                    None => {
                        errors.push(format!("bad_external_function: {parent_hash} missing abi"))
                    }
                }
                match payload.get("link_name").and_then(JsonValue::as_str) {
                    Some(link_name) => {
                        if let Err(err) = validate_external_link_name(link_name) {
                            errors.push(format!(
                                "bad_external_function: {parent_hash} invalid link_name: {err:#}"
                            ));
                        }
                    }
                    None => errors.push(format!(
                        "bad_external_function: {parent_hash} missing link_name"
                    )),
                }
                if let Some(library) = payload.get("library") {
                    match library.as_str() {
                        Some(library) => {
                            if let Err(err) = validate_external_library_name(library) {
                                errors.push(format!(
                                    "bad_external_function: {parent_hash} invalid library: {err:#}"
                                ));
                            }
                        }
                        None => errors.push(format!(
                            "bad_external_function: {parent_hash} library must be string"
                        )),
                    }
                }
            }
            "FunctionInterface" => {
                self.check_hash_ref(
                    parent_hash,
                    "symbol_hash",
                    payload.get("symbol_hash"),
                    errors,
                )?;
                self.check_hash_ref(
                    parent_hash,
                    "signature_hash",
                    payload.get("signature_hash"),
                    errors,
                )?;
            }
            "ProgramRoot" => {
                for (idx, entry) in payload
                    .get("symbols")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("symbols[{idx}].symbol"),
                        entry.get("symbol"),
                        errors,
                    )?;
                    self.check_hash_ref(
                        parent_hash,
                        &format!("symbols[{idx}].definition"),
                        entry.get("definition"),
                        errors,
                    )?;
                    self.check_hash_ref(
                        parent_hash,
                        &format!("symbols[{idx}].signature"),
                        entry.get("signature"),
                        errors,
                    )?;
                }
                for (idx, entry) in payload
                    .get("names")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("names[{idx}].symbol"),
                        entry.get("symbol"),
                        errors,
                    )?;
                }
                for (idx, entry) in payload
                    .get("types")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("types[{idx}].type_symbol"),
                        entry.get("type_symbol"),
                        errors,
                    )?;
                    self.check_hash_ref(
                        parent_hash,
                        &format!("types[{idx}].type_def"),
                        entry.get("type_def"),
                        errors,
                    )?;
                }
                for (idx, entry) in payload
                    .get("type_names")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("type_names[{idx}].type_symbol"),
                        entry.get("type_symbol"),
                        errors,
                    )?;
                }
                for (idx, entry) in payload
                    .get("param_names")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("param_names[{idx}].symbol"),
                        entry.get("symbol"),
                        errors,
                    )?;
                }
                for (idx, entry) in payload
                    .get("exports")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("exports[{idx}].symbol"),
                        entry.get("symbol"),
                        errors,
                    )?;
                }
                for (idx, entry) in payload
                    .get("tests")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("tests[{idx}].test"),
                        entry.get("test"),
                        errors,
                    )?;
                }
            }
            "TestCase" => {
                self.check_hash_ref(
                    parent_hash,
                    "entry_symbol",
                    payload.get("entry_symbol"),
                    errors,
                )?;
            }
            "LinkPlanInput" => {
                self.check_hash_ref(
                    parent_hash,
                    "entry_symbol_hash",
                    payload.get("entry_symbol_hash"),
                    errors,
                )?;
                for (idx, entry) in payload
                    .get("export_map")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("export_map[{idx}].symbol_hash"),
                        entry.get("symbol_hash"),
                        errors,
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn check_signature_effects(
        &self,
        parent_hash: &str,
        value: Option<&JsonValue>,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        let Some(value) = value else {
            return Ok(());
        };
        let Some(values) = value.as_array() else {
            errors.push(format!(
                "bad_signature_effects: {parent_hash} effects is not array"
            ));
            return Ok(());
        };
        let mut effects = Vec::new();
        let mut stored_names = Vec::new();
        let mut has_parse_error = false;
        for effect in values {
            let Some(effect) = effect.as_str() else {
                errors.push(format!(
                    "bad_signature_effects: {parent_hash} effect is not string"
                ));
                has_parse_error = true;
                continue;
            };
            stored_names.push(effect.to_string());
            match crate::types::Effect::from_str(effect) {
                Ok(effect) => effects.push(effect),
                Err(_) => {
                    has_parse_error = true;
                    errors.push(format!(
                        "bad_signature_effects: {parent_hash} unknown effect {effect}"
                    ));
                }
            }
        }
        match crate::types::normalize_effects(&effects) {
            Ok(normalized) if !has_parse_error => {
                let canonical_names = effect_names(&normalized)
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if stored_names != canonical_names {
                    errors.push(format!(
                        "bad_signature_effects: {parent_hash} effects are not canonical"
                    ));
                }
            }
            Ok(_) => {}
            Err(err) => errors.push(format!("bad_signature_effects: {parent_hash} {err:#}")),
        }
        Ok(())
    }

    fn check_hash_array_refs(
        &self,
        parent_hash: &str,
        field: &str,
        value: Option<&JsonValue>,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        for (idx, value) in value
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            self.check_hash_ref(parent_hash, &format!("{field}[{idx}]"), Some(value), errors)?;
        }
        Ok(())
    }

    fn verify_type_object_references(
        &self,
        parent_hash: &str,
        payload: &JsonValue,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        match payload.get("type_kind").and_then(JsonValue::as_str) {
            Some("I64" | "Bool" | "Unit") => {}
            Some("Named") => {
                self.check_hash_ref(
                    parent_hash,
                    "type_symbol",
                    payload.get("type_symbol"),
                    errors,
                )?;
                if let Some(args) = payload.get("region_args").and_then(JsonValue::as_array) {
                    for (idx, arg) in args.iter().enumerate() {
                        self.check_hash_ref(
                            parent_hash,
                            &format!("region_args[{idx}]"),
                            Some(arg),
                            errors,
                        )?;
                    }
                }
            }
            Some("Reference") => {
                self.check_hash_ref(parent_hash, "region", payload.get("region"), errors)?;
                self.check_hash_ref(parent_hash, "referent", payload.get("referent"), errors)?;
            }
            Some("RawPointer") => {
                self.check_hash_ref(parent_hash, "pointee", payload.get("pointee"), errors)?;
            }
            Some("Slice") => {
                self.check_hash_ref(parent_hash, "region", payload.get("region"), errors)?;
                self.check_hash_ref(parent_hash, "element", payload.get("element"), errors)?;
            }
            Some("FixedArray") => {
                self.check_hash_ref(parent_hash, "element", payload.get("element"), errors)?;
            }
            Some("Record") => {
                for (idx, field) in payload
                    .get("fields")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("fields[{idx}].type"),
                        field.get("type"),
                        errors,
                    )?;
                }
            }
            Some("Enum") => {
                for (idx, variant) in payload
                    .get("variants")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("variants[{idx}].type"),
                        variant.get("type"),
                        errors,
                    )?;
                }
            }
            Some(_) | None => {}
        }
        Ok(())
    }

    fn verify_type_definition_payload(
        &self,
        parent_hash: &str,
        label: &str,
        members_key: &str,
        member_symbol_key: &str,
        payload: &JsonValue,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        let mut allowed_regions = BTreeSet::new();
        match region_params_from_payload(payload.get("region_params")) {
            Ok(params) => {
                if let Err(err) = validate_region_params(&params) {
                    errors.push(format!("bad_type_def: {parent_hash}: {err:#}"));
                }
                for (idx, param) in params.iter().enumerate() {
                    allowed_regions.insert(param.region.clone());
                    self.check_hash_ref(
                        parent_hash,
                        &format!("region_params[{idx}].region"),
                        Some(&JsonValue::String(param.region.clone())),
                        errors,
                    )?;
                }
            }
            Err(err) => errors.push(format!("bad_type_def: {parent_hash}: {err:#}")),
        }
        match member_defs_from_payload(label, member_symbol_key, payload.get(members_key)) {
            Ok(members) => {
                if let Err(err) = validate_member_defs(label, &members) {
                    errors.push(format!("bad_type_def: {parent_hash}: {err:#}"));
                }
                for (idx, member) in members.iter().enumerate() {
                    self.check_hash_ref(
                        parent_hash,
                        &format!("{members_key}[{idx}].{member_symbol_key}"),
                        Some(&JsonValue::String(member.member_symbol.clone())),
                        errors,
                    )?;
                    self.check_hash_ref(
                        parent_hash,
                        &format!("{members_key}[{idx}].type"),
                        Some(&JsonValue::String(member.type_hash.clone())),
                        errors,
                    )?;
                    self.verify_type_region_args(
                        parent_hash,
                        &member.type_hash,
                        &allowed_regions,
                        errors,
                    )?;
                }
            }
            Err(err) => errors.push(format!("bad_type_def: {parent_hash}: {err:#}")),
        }
        Ok(())
    }

    fn verify_type_region_args(
        &self,
        parent_hash: &str,
        type_hash: &str,
        allowed_regions: &BTreeSet<String>,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        let Ok(payload) = self.get_payload(type_hash) else {
            return Ok(());
        };
        match type_spec_from_payload(&payload) {
            Ok(crate::types::TypeSpec::Named { region_args, .. }) => {
                for region in region_args {
                    if !allowed_regions.contains(&region) {
                        errors.push(format!(
                            "bad_type_def: {parent_hash}: invalid region reference {region}"
                        ));
                    }
                }
            }
            Ok(crate::types::TypeSpec::Reference {
                region, referent, ..
            }) => {
                if !allowed_regions.contains(&region) {
                    errors.push(format!(
                        "bad_type_def: {parent_hash}: invalid region reference {region}"
                    ));
                }
                self.verify_type_region_args(parent_hash, &referent, allowed_regions, errors)?;
            }
            Ok(crate::types::TypeSpec::RawPointer { pointee, .. }) => {
                self.verify_type_region_args(parent_hash, &pointee, allowed_regions, errors)?;
            }
            Ok(crate::types::TypeSpec::Slice {
                region, element, ..
            }) => {
                if !allowed_regions.contains(&region) {
                    errors.push(format!(
                        "bad_type_def: {parent_hash}: invalid region reference {region}"
                    ));
                }
                self.verify_type_region_args(parent_hash, &element, allowed_regions, errors)?;
            }
            Ok(crate::types::TypeSpec::FixedArray { element, .. }) => {
                self.verify_type_region_args(parent_hash, &element, allowed_regions, errors)?;
            }
            Ok(crate::types::TypeSpec::Record(fields))
            | Ok(crate::types::TypeSpec::Enum(fields)) => {
                for field in fields {
                    self.verify_type_region_args(
                        parent_hash,
                        &field.type_hash,
                        allowed_regions,
                        errors,
                    )?;
                }
            }
            Ok(crate::types::TypeSpec::Builtin(_)) => {}
            Err(err) => errors.push(format!("bad_type_def: {parent_hash}: {err:#}")),
        }
        Ok(())
    }

    fn check_hash_ref(
        &self,
        parent_hash: &str,
        field: &str,
        value: Option<&JsonValue>,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        let Some(child_hash) = value.and_then(JsonValue::as_str) else {
            return Ok(());
        };
        if !child_hash.starts_with("sha256:") {
            return Ok(());
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
            params![child_hash],
            |row| row.get(0),
        )?;
        if !exists {
            errors.push(format!(
                "missing_object: {parent_hash} field {field} references missing object {child_hash}"
            ));
        }
        Ok(())
    }

    fn verify_history_replay_readonly(&mut self, errors: &mut Vec<String>) -> Result<()> {
        let has_main_branch: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM branches WHERE name = ?1)",
            params![crate::MAIN_BRANCH],
            |row| row.get(0),
        )?;
        if !has_main_branch {
            return Ok(());
        }
        self.conn.execute_batch("SAVEPOINT verify_replay")?;
        let replay_result = self.replay_main_branch_without_init();
        let rollback_result = self
            .conn
            .execute_batch("ROLLBACK TO verify_replay; RELEASE verify_replay");
        if let Err(err) = rollback_result {
            bail!("verify replay rollback failed: {err}");
        }
        if let Err(err) = replay_result {
            let message = format!("{err:#}");
            if message.starts_with("bad_history_link") || message.starts_with("semantic_conflict") {
                errors.push(message);
            } else {
                errors.push(format!("bad_history_link: {message}"));
            }
        }
        Ok(())
    }

    fn verify_edges(&self, errors: &mut Vec<String>) -> Result<()> {
        let missing_parent: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM object_edges e
             LEFT JOIN objects o ON o.hash = e.parent_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        let missing_child: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM object_edges e
             LEFT JOIN objects o ON o.hash = e.child_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        if missing_parent > 0 || missing_child > 0 {
            errors.push(format!(
                "missing_object: object_edges missing parents={missing_parent} children={missing_child}"
            ));
        }

        let mut expected = BTreeSet::new();
        let mut stmt = self
            .conn
            .prepare("SELECT hash, payload_json FROM objects ORDER BY hash")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (parent_hash, payload_json) = row?;
            let payload = match serde_json::from_str::<JsonValue>(&payload_json) {
                Ok(payload) => payload,
                Err(err) => {
                    errors.push(format!(
                        "corrupt_object: cannot recompute edges for {parent_hash}: {err}"
                    ));
                    continue;
                }
            };
            let mut refs = Vec::new();
            extract_hash_strings(&payload, &mut refs);
            let mut seen = BTreeSet::new();
            for (position, child_hash) in refs.into_iter().enumerate() {
                if !seen.insert(child_hash.clone()) || child_hash == parent_hash {
                    continue;
                }
                let exists: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                    params![&child_hash],
                    |row| row.get(0),
                )?;
                if exists {
                    expected.insert((
                        parent_hash.clone(),
                        child_hash,
                        "ref".to_string(),
                        Some(position as i64),
                    ));
                }
            }
        }
        drop(stmt);

        let actual = {
            let mut stmt = self.conn.prepare(
                "SELECT parent_hash, child_hash, edge_label, edge_position
                 FROM object_edges
                 ORDER BY parent_hash, child_hash, edge_label, edge_position",
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected != actual {
            errors.push("bad_index: object_edges mismatch".to_string());
        }
        Ok(())
    }

    fn verify_branches(&self, errors: &mut Vec<String>) -> Result<()> {
        let program_root_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM objects WHERE kind = 'ProgramRoot'",
            [],
            |row| row.get(0),
        )?;
        let main_branch_exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM branches WHERE name = ?1)",
            params![crate::MAIN_BRANCH],
            |row| row.get(0),
        )?;
        if program_root_count > 0 && !main_branch_exists {
            errors.push("bad_index: main branch is missing".to_string());
        }

        let missing_roots: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM branches b
             LEFT JOIN objects o ON o.hash = b.root_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        if missing_roots > 0 {
            errors.push(format!(
                "missing_object: branch roots missing {missing_roots}"
            ));
        }
        let missing_histories: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM branches b
             LEFT JOIN histories h ON h.history_hash = b.history_hash
             WHERE b.history_hash IS NOT NULL AND h.history_hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        if missing_histories > 0 {
            errors.push(format!(
                "bad_history_link: branch histories missing {missing_histories}"
            ));
        }
        let mismatched_histories: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM branches b
             JOIN histories h ON h.history_hash = b.history_hash
             WHERE b.history_hash IS NOT NULL
               AND h.output_root_hash != b.root_hash",
            [],
            |row| row.get(0),
        )?;
        if mismatched_histories > 0 {
            errors.push(format!(
                "bad_history_link: branch histories output wrong root {mismatched_histories}"
            ));
        }
        Ok(())
    }

    fn verify_migrations_and_histories(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut migration_outputs = BTreeMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT hash, parent_history_hash, input_root_hash, output_root_hash,
                    operation_kind, operation_json, preconditions_json, postconditions_json
             FROM migrations ORDER BY created_at, hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        for row in rows {
            let (
                hash,
                parent_history,
                input_root,
                output_root,
                operation_kind,
                operation_json,
                preconditions_json,
                postconditions_json,
            ) = row?;
            let operation_value = serde_json::from_str::<JsonValue>(&operation_json);
            let operation = serde_json::from_str::<Operation>(&operation_json);
            let preconditions = serde_json::from_str::<JsonValue>(&preconditions_json);
            let postconditions = serde_json::from_str::<JsonValue>(&postconditions_json);
            match (operation_value, operation, preconditions, postconditions) {
                (Ok(operation_value), Ok(operation), Ok(preconditions), Ok(postconditions)) => {
                    if operation.kind_name() != operation_kind {
                        errors.push(format!(
                            "bad_history_link: migration {hash} operation kind {operation_kind} does not match operation {}",
                            operation.kind_name()
                        ));
                    }
                    let recomputed = migration_hash(
                        parent_history.as_deref(),
                        &input_root,
                        &output_root,
                        &operation_value,
                        &preconditions,
                        &postconditions,
                    );
                    if recomputed != hash {
                        errors.push(format!(
                            "bad_hash: migration {hash} recomputes to {recomputed}"
                        ));
                    }
                }
                _ => errors.push(format!("corrupt_object: migration json invalid {hash}")),
            }
            migration_outputs.insert(hash.clone(), (parent_history.clone(), output_root.clone()));
            for root in [input_root, output_root] {
                let exists: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                    params![root],
                    |row| row.get(0),
                )?;
                if !exists {
                    errors.push(format!(
                        "missing_object: migration {hash} references missing root {root}"
                    ));
                }
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT history_hash, parent_history_hash, migration_hash, output_root_hash
             FROM histories ORDER BY created_at, history_hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (history, parent, migration, output_root) = row?;
            let recomputed = history_hash(parent.as_deref(), &migration, &output_root);
            if recomputed != history {
                errors.push(format!(
                    "bad_history_link: {history} recomputes to {recomputed}"
                ));
            }
            match migration_outputs.get(&migration) {
                Some((migration_parent, migration_output)) => {
                    if migration_parent != &parent {
                        errors.push(format!(
                            "bad_history_link: history {history} parent does not match migration {migration} parent"
                        ));
                    }
                    if migration_output != &output_root {
                        errors.push(format!(
                            "bad_history_link: history {history} output root {output_root} does not match migration {migration} output root {migration_output}"
                        ));
                    }
                }
                None => errors.push(format!(
                    "bad_history_link: history {history} references missing migration {migration}"
                )),
            }
            let output_exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                params![&output_root],
                |row| row.get(0),
            )?;
            if !output_exists {
                errors.push(format!(
                    "missing_object: history {history} references missing root {output_root}"
                ));
            }
        }
        Ok(())
    }

    fn verify_roots(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM objects WHERE kind = 'ProgramRoot' ORDER BY hash")?;
        let root_hashes = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        for root_hash in root_hashes {
            match self.type_check_root(&root_hash) {
                Ok(()) => {}
                Err(err) => errors.push(format!("bad_type: root {root_hash}: {err:#}")),
            }
            let root = match self.load_root(&root_hash) {
                Ok(root) => root,
                Err(err) => {
                    errors.push(format!("corrupt_object: root {root_hash}: {err:#}"));
                    continue;
                }
            };
            if let Err(err) = self.verify_root_indexes(&root_hash, &root, errors) {
                errors.push(format!("bad_index: root {root_hash}: {err:#}"));
            }
        }
        Ok(())
    }

    fn verify_root_indexes(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        let expected_symbols = root
            .symbols
            .iter()
            .map(|entry| {
                (
                    entry.symbol.clone(),
                    entry.definition.clone(),
                    entry.signature.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let actual_symbols = {
            let mut stmt = self.conn.prepare(
                "SELECT symbol_hash, definition_hash, signature_hash FROM root_symbols
                 WHERE root_hash = ?1 ORDER BY symbol_hash",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_symbols != actual_symbols {
            errors.push(format!("bad_index: root_symbols mismatch for {root_hash}"));
        }

        let expected_types = root
            .types
            .iter()
            .map(|entry| (entry.type_symbol.clone(), entry.type_def.clone()))
            .collect::<BTreeSet<_>>();
        let actual_types = {
            let mut stmt = self.conn.prepare(
                "SELECT type_symbol_hash, type_def_hash FROM root_types
                 WHERE root_hash = ?1 ORDER BY type_symbol_hash",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_types != actual_types {
            errors.push(format!("bad_index: root_types mismatch for {root_hash}"));
        }

        let expected_names = root
            .names
            .iter()
            .map(|binding| {
                (
                    binding.module.clone(),
                    binding.display_name.clone(),
                    binding.symbol.clone(),
                    binding.is_preferred,
                )
            })
            .collect::<BTreeSet<_>>();
        let actual_names = {
            let mut stmt = self.conn.prepare(
                "SELECT module_name, display_name, symbol_hash, is_preferred FROM root_names
                 WHERE root_hash = ?1 ORDER BY module_name, display_name",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_names != actual_names {
            errors.push(format!("bad_index: root_names mismatch for {root_hash}"));
        }

        let expected_type_names = root
            .type_names
            .iter()
            .map(|binding| {
                (
                    binding.module.clone(),
                    binding.display_name.clone(),
                    binding.type_symbol.clone(),
                    binding.is_preferred,
                )
            })
            .collect::<BTreeSet<_>>();
        let actual_type_names = {
            let mut stmt = self.conn.prepare(
                "SELECT module_name, display_name, type_symbol_hash, is_preferred FROM root_type_names
                 WHERE root_hash = ?1 ORDER BY module_name, display_name",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_type_names != actual_type_names {
            errors.push(format!(
                "bad_index: root_type_names mismatch for {root_hash}"
            ));
        }
        self.verify_projection_names(root_hash, root, errors);
        self.verify_module_metadata(root_hash, root, errors);

        let root_symbols = root
            .symbols
            .iter()
            .map(|entry| entry.symbol.clone())
            .collect::<BTreeSet<_>>();
        let mut seen_exported_names = BTreeSet::new();
        for export in &root.exports {
            if !root_symbols.contains(&export.symbol) {
                errors.push(format!(
                    "bad_abi_symbol: export {} points to missing symbol {} in {root_hash}",
                    export.exported_name, export.symbol
                ));
            }
            if let Err(err) = validate_exported_abi_name(&export.exported_name) {
                errors.push(format!(
                    "bad_abi_symbol: invalid export {} in {root_hash}: {err:#}",
                    export.exported_name
                ));
            }
            if !seen_exported_names.insert(export.exported_name.clone()) {
                errors.push(format!(
                    "bad_abi_symbol: duplicate export {} in {root_hash}",
                    export.exported_name
                ));
            }
        }
        if let Err(err) = validate_export_map(root) {
            errors.push(format!("bad_abi_symbol: root {root_hash}: {err:#}"));
        }

        let expected_exports = root
            .exports
            .iter()
            .map(|binding| (binding.exported_name.clone(), binding.symbol.clone()))
            .collect::<BTreeSet<_>>();
        let actual_exports = {
            let mut stmt = self.conn.prepare(
                "SELECT exported_name, symbol_hash FROM root_exports
                 WHERE root_hash = ?1 ORDER BY exported_name",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_exports != actual_exports {
            errors.push(format!("bad_index: root_exports mismatch for {root_hash}"));
        }

        let mut expected_deps = BTreeSet::new();
        for entry in &root.symbols {
            for dep in self.dependencies_for_definition(root, &entry.definition)? {
                expected_deps.insert((entry.symbol.clone(), dep));
            }
        }
        let actual_deps = dependency_pairs(&self.conn, root_hash)?;
        if expected_deps != actual_deps {
            errors.push(format!(
                "bad_dependency_index: dependencies mismatch for {root_hash}"
            ));
        }
        self.verify_source_search_index(root_hash, root, errors)?;
        Ok(())
    }

    fn verify_source_search_index(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        let mut expected = BTreeMap::new();
        for binding in preferred_names(root) {
            let symbol = binding.symbol.clone();
            if let Some(entry) = self.root_symbol(root, &symbol) {
                let source = self.render_function_source(root, &binding, entry)?;
                *expected.entry((symbol, source)).or_insert(0) += 1;
            }
        }
        let actual = {
            let mut stmt = self.conn.prepare(
                "SELECT symbol_hash, rendered_source FROM source_search
                 WHERE root_hash = ?1 ORDER BY symbol_hash, rendered_source",
            )?;
            let rows = stmt.query_map(params![root_hash], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut actual = BTreeMap::new();
            for row in rows {
                *actual.entry(row?).or_insert(0) += 1;
            }
            actual
        };
        if expected != actual {
            errors.push(format!("bad_index: source_search mismatch for {root_hash}"));
        }
        Ok(())
    }

    fn verify_projection_names(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        errors: &mut Vec<String>,
    ) {
        for binding in &root.names {
            if let Err(err) = validate_module_path("module", &binding.module) {
                errors.push(format!(
                    "bad_index: invalid module for {} in {root_hash}: {err:#}",
                    binding.symbol
                ));
            }
            if let Err(err) = validate_projection_identifier("display name", &binding.display_name)
            {
                errors.push(format!(
                    "bad_index: invalid display name for {} in {root_hash}: {err:#}",
                    binding.symbol
                ));
            }
        }
        for binding in &root.type_names {
            if let Err(err) = validate_module_path("module", &binding.module) {
                errors.push(format!(
                    "bad_index: invalid module for type {} in {root_hash}: {err:#}",
                    binding.type_symbol
                ));
            }
            if let Err(err) = validate_projection_identifier("type name", &binding.display_name) {
                errors.push(format!(
                    "bad_index: invalid type name for {} in {root_hash}: {err:#}",
                    binding.type_symbol
                ));
            }
        }
        for entry in &root.param_names {
            let mut seen = BTreeSet::new();
            for name in &entry.names {
                if let Err(err) = validate_projection_identifier("parameter name", name) {
                    errors.push(format!(
                        "bad_index: invalid parameter name for {} in {root_hash}: {err:#}",
                        entry.symbol
                    ));
                }
                if !seen.insert(name.clone()) {
                    errors.push(format!(
                        "bad_index: duplicate parameter name {name:?} for {} in {root_hash}",
                        entry.symbol
                    ));
                }
            }
        }
    }

    fn verify_module_metadata(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        errors: &mut Vec<String>,
    ) {
        let Some(value) = root.metadata.get(ROOT_MODULES_METADATA_KEY) else {
            return;
        };
        let Some(entries) = value.as_array() else {
            errors.push(format!(
                "bad_index: modules metadata must be an array in {root_hash}"
            ));
            return;
        };
        let actual_modules = root
            .names
            .iter()
            .map(|binding| binding.module.clone())
            .chain(root.type_names.iter().map(|binding| binding.module.clone()))
            .collect::<BTreeSet<_>>();
        let mut metadata_modules = BTreeSet::new();
        for entry in entries {
            let Some(name) = entry.get("name").and_then(JsonValue::as_str) else {
                errors.push(format!(
                    "bad_index: module metadata entry missing name in {root_hash}"
                ));
                continue;
            };
            if let Err(err) = validate_module_path("module", name) {
                errors.push(format!(
                    "bad_index: invalid module metadata {name:?} in {root_hash}: {err:#}"
                ));
            }
            if !metadata_modules.insert(name.to_string()) {
                errors.push(format!(
                    "bad_index: duplicate module metadata {name:?} in {root_hash}"
                ));
            }
        }
        if metadata_modules != actual_modules {
            errors.push(format!(
                "bad_index: modules metadata mismatch for {root_hash}"
            ));
        }
    }

    fn verify_workspace_transactions(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT request_id, request_hash, method, branch, expected_root_hash, response_json
             FROM workspace_transactions ORDER BY request_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (request_id, request_hash, method, branch, expected_root, response_json) = row?;
            if request_id.trim().is_empty() {
                errors.push("bad_workspace_transaction: request_id must not be empty".to_string());
            }
            if request_hash.trim().is_empty() {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} request_hash must not be empty"
                ));
            }
            if method != "ops.apply" {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} method {method:?} is not supported"
                ));
            }
            if branch.trim().is_empty() {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} branch must not be empty"
                ));
            }
            if let Some(expected_root) = expected_root.as_deref() {
                self.check_existing_object_hash(
                    errors,
                    "bad_workspace_transaction",
                    &request_id,
                    "expected_root_hash",
                    expected_root,
                )?;
            }

            let response = match serde_json::from_str::<JsonValue>(&response_json) {
                Ok(response) => {
                    if canonical_json(&response) != response_json {
                        errors.push(format!(
                            "bad_workspace_transaction: {request_id} response_json is not canonical"
                        ));
                    }
                    response
                }
                Err(err) => {
                    errors.push(format!(
                        "bad_workspace_transaction: {request_id} response_json is invalid JSON: {err}"
                    ));
                    continue;
                }
            };
            if response.get("schema").and_then(JsonValue::as_str)
                != Some(crate::workspace::WORKSPACE_RESPONSE_SCHEMA)
            {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} response schema mismatch"
                ));
            }
            if response.get("status").and_then(JsonValue::as_str) != Some("ok") {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} cached response is not an ok response"
                ));
            }
            let Some(result) = response.get("result").and_then(JsonValue::as_object) else {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} response missing result"
                ));
                continue;
            };
            if result.get("schema").and_then(JsonValue::as_str) != Some("codedb/apply-result/v1") {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} result schema mismatch"
                ));
            }
            if result.get("committed").and_then(JsonValue::as_bool) != Some(true) {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} result is not committed"
                ));
            }
            if result.get("branch").and_then(JsonValue::as_str) != Some(branch.as_str()) {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} result branch does not match transaction row"
                ));
            }
            if let Some(expected_root) = expected_root.as_deref()
                && result.get("old_root_hash").and_then(JsonValue::as_str) != Some(expected_root)
            {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} expected root does not match result old_root_hash"
                ));
            }
            for field in ["old_root_hash", "new_root_hash"] {
                if let Some(root_hash) = result.get(field).and_then(JsonValue::as_str) {
                    self.check_existing_object_hash(
                        errors,
                        "bad_workspace_transaction",
                        &request_id,
                        field,
                        root_hash,
                    )?;
                }
            }
            if let Some(snapshot) = response.get("snapshot").and_then(JsonValue::as_object) {
                if snapshot.get("branch").and_then(JsonValue::as_str) != Some(branch.as_str()) {
                    errors.push(format!(
                        "bad_workspace_transaction: {request_id} snapshot branch does not match transaction row"
                    ));
                }
                if let Some(new_root) = result.get("new_root_hash").and_then(JsonValue::as_str)
                    && snapshot.get("root_hash").and_then(JsonValue::as_str) != Some(new_root)
                {
                    errors.push(format!(
                        "bad_workspace_transaction: {request_id} snapshot root does not match result new_root_hash"
                    ));
                }
            } else {
                errors.push(format!(
                    "bad_workspace_transaction: {request_id} response missing snapshot"
                ));
            }
        }
        Ok(())
    }

    fn verify_artifact_jobs(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT cache_key, artifact_kind, status, worker_id, started_at, finished_at, error_json
             FROM artifact_jobs ORDER BY cache_key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        for row in rows {
            let (cache_key, artifact_kind, status, worker_id, started_at, finished_at, error_json) =
                row?;
            let Some(parsed_kind) = ArtifactKind::from_str(&artifact_kind) else {
                errors.push(format!(
                    "bad_artifact_job: {cache_key} has unknown artifact kind {artifact_kind}"
                ));
                continue;
            };
            match status.as_str() {
                "queued" => {
                    if worker_id.is_some()
                        || started_at.is_some()
                        || finished_at.is_some()
                        || error_json.is_some()
                    {
                        errors.push(format!(
                            "bad_artifact_job: {cache_key} queued job has worker/timestamp/error metadata"
                        ));
                    }
                }
                "running" => {
                    if worker_id.is_none() || started_at.is_none() {
                        errors.push(format!(
                            "bad_artifact_job: {cache_key} running job is missing worker or started_at"
                        ));
                    }
                    if finished_at.is_some() || error_json.is_some() {
                        errors.push(format!(
                            "bad_artifact_job: {cache_key} running job has finished/error metadata"
                        ));
                    }
                }
                "succeeded" => {
                    if worker_id.is_none() || started_at.is_none() || finished_at.is_none() {
                        errors.push(format!(
                            "bad_artifact_job: {cache_key} succeeded job is missing worker or timestamps"
                        ));
                    }
                    if error_json.is_some() {
                        errors.push(format!(
                            "bad_artifact_job: {cache_key} succeeded job has error metadata"
                        ));
                    }
                    let cache_artifact_kind = self
                        .conn
                        .query_row(
                            "SELECT artifact_kind FROM compile_cache WHERE cache_key = ?1",
                            params![&cache_key],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?;
                    if let Some(cache_artifact_kind) = cache_artifact_kind
                        && cache_artifact_kind != artifact_kind
                    {
                        errors.push(format!(
                            "bad_artifact_job: {cache_key} succeeded job kind {artifact_kind} does not match cache kind {cache_artifact_kind}"
                        ));
                    }
                }
                "failed" | "abandoned" => {
                    if worker_id.is_none() || started_at.is_none() || finished_at.is_none() {
                        errors.push(format!(
                            "bad_artifact_job: {cache_key} {status} job is missing worker or timestamps"
                        ));
                    }
                    self.verify_artifact_job_error(errors, &cache_key, error_json.as_deref());
                }
                other => errors.push(format!(
                    "bad_artifact_job: {cache_key} has unknown status {other:?}"
                )),
            }
            if parsed_kind.requires_artifact_bytes() && status == "succeeded" {
                let has_bytes: Option<bool> = self
                    .conn
                    .query_row(
                        "SELECT artifact_bytes IS NOT NULL FROM compile_cache WHERE cache_key = ?1",
                        params![&cache_key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if has_bytes == Some(false) {
                    errors.push(format!(
                        "bad_artifact_job: {cache_key} succeeded bytes job has cache entry without bytes"
                    ));
                }
            }
        }
        Ok(())
    }

    fn verify_artifact_job_error(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        error_json: Option<&str>,
    ) {
        let Some(error_json) = error_json else {
            errors.push(format!(
                "bad_artifact_job: {cache_key} failed/abandoned job is missing error_json"
            ));
            return;
        };
        let value = match serde_json::from_str::<JsonValue>(error_json) {
            Ok(value) => {
                if canonical_json(&value) != error_json {
                    errors.push(format!(
                        "bad_artifact_job: {cache_key} error_json is not canonical"
                    ));
                }
                value
            }
            Err(err) => {
                errors.push(format!(
                    "bad_artifact_job: {cache_key} error_json is invalid JSON: {err}"
                ));
                return;
            }
        };
        if value.get("schema").and_then(JsonValue::as_str) != Some("codedb/artifact-job-error/v1") {
            errors.push(format!(
                "bad_artifact_job: {cache_key} error_json schema mismatch"
            ));
        }
        if value.get("kind").and_then(JsonValue::as_str).is_none() {
            errors.push(format!(
                "bad_artifact_job: {cache_key} error_json missing kind"
            ));
        }
        if value.get("message").and_then(JsonValue::as_str).is_none() {
            errors.push(format!(
                "bad_artifact_job: {cache_key} error_json missing message"
            ));
        }
    }

    fn check_existing_object_hash(
        &self,
        errors: &mut Vec<String>,
        kind: &str,
        owner: &str,
        field: &str,
        hash: &str,
    ) -> Result<()> {
        if !hash.starts_with("sha256:") {
            errors.push(format!("{kind}: {owner} {field} is not a sha256 hash"));
            return Ok(());
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
            params![hash],
            |row| row.get(0),
        )?;
        if !exists {
            errors.push(format!(
                "{kind}: {owner} {field} references missing object {hash}"
            ));
        }
        Ok(())
    }

    fn verify_caches(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT cache_key, cache_key_json, input_hash, backend, target, compiler_version,
                    artifact_kind, artifact_hash, artifact_json, artifact_bytes
             FROM compile_cache ORDER BY cache_key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<Vec<u8>>>(9)?,
            ))
        })?;
        for row in rows {
            let (
                cache_key,
                cache_key_json,
                input_hash,
                backend,
                target,
                compiler_version,
                artifact_kind,
                artifact_hash,
                artifact_json,
                artifact_bytes,
            ) = row?;
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                params![&input_hash],
                |row| row.get(0),
            )?;
            if !exists {
                errors.push(format!(
                    "bad_cache_entry: {cache_key} references missing input {input_hash}"
                ));
            }
            let Some(artifact_kind) = ArtifactKind::from_str(&artifact_kind) else {
                errors.push(format!(
                    "bad_cache_entry: {cache_key} has unknown artifact kind {artifact_kind}"
                ));
                continue;
            };

            let mut parsed_key_input = None;
            match cache_key_json {
                Some(cache_key_json) => match serde_json::from_str::<JsonValue>(&cache_key_json) {
                    Ok(value) => {
                        if canonical_json(&value) != cache_key_json {
                            errors.push(format!(
                                "bad_cache_entry: cache key payload is not canonical {cache_key}"
                            ));
                        }
                        match serde_json::from_value::<CacheKeyInput>(value) {
                            Ok(key_input) => {
                                let key_input = key_input.normalized();
                                if let Err(err) = key_input.validate() {
                                    errors.push(format!(
                                        "bad_cache_entry: invalid cache key payload {cache_key}: {err:#}"
                                    ));
                                } else {
                                    if key_input.artifact_kind != artifact_kind {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key artifact kind mismatch {cache_key}"
                                        ));
                                    }
                                    if key_input.input_hash != input_hash {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key input mismatch {cache_key}"
                                        ));
                                    }
                                    if key_input.backend_id != backend {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key backend mismatch {cache_key}"
                                        ));
                                    }
                                    if key_input.target_triple != target {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key target mismatch {cache_key}"
                                        ));
                                    }
                                    if key_input.compiler_version != compiler_version {
                                        errors.push(format!(
                                            "bad_cache_entry: cache key compiler version mismatch {cache_key}"
                                        ));
                                    }
                                    match cache_key_for_input(&key_input) {
                                        Ok(recomputed) if recomputed != cache_key => {
                                            errors.push(format!(
                                                "bad_cache_entry: cache key mismatch {cache_key} recomputes to {recomputed}"
                                            ));
                                        }
                                        Ok(_) => {
                                            parsed_key_input = Some(key_input);
                                        }
                                        Err(err) => errors.push(format!(
                                            "bad_cache_entry: cannot recompute cache key {cache_key}: {err:#}"
                                        )),
                                    }
                                }
                            }
                            Err(err) => errors.push(format!(
                                "bad_cache_entry: invalid cache key json {cache_key}: {err}"
                            )),
                        }
                    }
                    Err(err) => errors.push(format!(
                        "bad_cache_entry: invalid cache key json {cache_key}: {err}"
                    )),
                },
                None => errors.push(format!(
                    "bad_cache_entry: missing cache key payload {cache_key}"
                )),
            }

            let artifact_value = match artifact_json.as_deref() {
                Some(artifact_json) => match serde_json::from_str::<JsonValue>(artifact_json) {
                    Ok(value) => {
                        if canonical_json(&value) != artifact_json {
                            errors.push(format!(
                                "bad_cache_entry: artifact_json is not canonical {cache_key}"
                            ));
                        }
                        Some(value)
                    }
                    Err(err) => {
                        errors.push(format!("bad_cache_entry: {cache_key}: {err}"));
                        None
                    }
                },
                None => None,
            };

            if artifact_kind.requires_artifact_bytes() && artifact_bytes.is_none() {
                errors.push(format!(
                    "bad_artifact_bytes: {cache_key} missing artifact bytes for {artifact_kind}"
                ));
            }

            if let Some(value) = artifact_value.as_ref() {
                verify_artifact_metadata(
                    errors,
                    ArtifactMetadataCheck {
                        cache_key: &cache_key,
                        artifact_kind,
                        input_hash: &input_hash,
                        backend: &backend,
                        target: &target,
                        artifact_hash: &artifact_hash,
                        artifact_json: value,
                        artifact_bytes: artifact_bytes.as_deref(),
                    },
                );
            } else if let Some(bytes) = artifact_bytes.as_deref() {
                let recomputed = hash_bytes(BYTES_DOMAIN, bytes);
                if recomputed != artifact_hash {
                    errors.push(format!(
                        "bad_artifact_bytes: {cache_key} artifact bytes hash {artifact_hash} recomputes to {recomputed}"
                    ));
                }
            }

            if artifact_kind == ArtifactKind::CProjection
                && let Some(value) = artifact_value.as_ref()
                && let Some(text) = artifact_text(value)
                && let Err(err) = ensure_no_forbidden_runtime_calls(text)
            {
                errors.push(format!(
                    "forbidden_runtime_dependency: {cache_key}: {err:#}"
                ));
            }

            if artifact_kind == ArtifactKind::LoweredIr
                && let Some(value) = artifact_value.as_ref()
            {
                match crate::lowering::lowered_ir_from_artifact_metadata(value) {
                    Ok(ir) => {
                        if let Err(err) =
                            self.verify_lowered_ir_against_index(&input_hash, &target, &ir)
                        {
                            errors.push(format!("bad_lowered_ir: {cache_key}: {err:#}"));
                        }
                    }
                    Err(err) => errors.push(format!("bad_lowered_ir: {cache_key}: {err:#}")),
                }
            }

            if let (Some(key_input), Some(value)) =
                (parsed_key_input.as_ref(), artifact_value.as_ref())
            {
                match artifact_kind {
                    ArtifactKind::TypedExpression => {
                        self.verify_typed_expression_artifact(
                            errors, &cache_key, key_input, value,
                        )?;
                    }
                    ArtifactKind::FunctionDependencySet => {
                        self.verify_function_dependency_set_artifact(
                            errors, &cache_key, key_input, value,
                        )?;
                    }
                    ArtifactKind::InterfaceHash => {
                        self.verify_interface_hash_artifact(errors, &cache_key, key_input, value)?;
                    }
                    ArtifactKind::ImplementationHash => {
                        self.verify_implementation_hash_artifact(
                            errors, &cache_key, key_input, value,
                        )?;
                    }
                    ArtifactKind::TypeLayout => {
                        self.verify_type_layout_artifact(errors, &cache_key, key_input, value)?;
                    }
                    ArtifactKind::ObjectFile => {
                        self.verify_object_artifact(
                            errors,
                            &cache_key,
                            key_input,
                            value,
                            artifact_bytes.as_deref(),
                        )?;
                    }
                    ArtifactKind::LinkPlan => {
                        self.verify_link_plan_artifact(errors, &cache_key, key_input, value)?;
                    }
                    ArtifactKind::Executable => {
                        self.verify_executable_artifact(errors, &cache_key, key_input, value)?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn verify_typed_expression_artifact(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let Some(metadata) = artifact_inner_metadata(artifact_json) else {
            errors.push(format!(
                "bad_typed_expression: {cache_key} missing expression metadata"
            ));
            return Ok(());
        };
        if !key_input.dependency_interface_hashes.is_empty()
            || !key_input.dependency_implementation_hashes.is_empty()
        {
            errors.push(format!(
                "bad_typed_expression: {cache_key} cache key should not record dependencies"
            ));
        }
        let payload = self.get_payload(&key_input.input_hash)?;
        if payload.get("type").and_then(JsonValue::as_str)
            != metadata.get("type").and_then(JsonValue::as_str)
        {
            errors.push(format!(
                "bad_typed_expression: {cache_key} type metadata does not match expression"
            ));
        }
        Ok(())
    }

    fn verify_function_dependency_set_artifact(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let Some(metadata) = artifact_inner_metadata(artifact_json) else {
            errors.push(format!(
                "bad_function_dependency_set: {cache_key} missing dependency metadata"
            ));
            return Ok(());
        };
        if !key_input.dependency_interface_hashes.is_empty()
            || !key_input.dependency_implementation_hashes.is_empty()
        {
            errors.push(format!(
                "bad_function_dependency_set: {cache_key} cache key should not record dependencies"
            ));
        }
        let Some(metadata_dependencies) = json_string_vec(metadata.get("dependencies")) else {
            errors.push(format!(
                "bad_function_dependency_set: {cache_key} dependencies must be a string array"
            ));
            return Ok(());
        };
        let indexed_dependencies =
            self.dependency_symbols_for_indexed_roots(&key_input.input_hash)?;
        if indexed_dependencies.is_empty() {
            errors.push(format!(
                "bad_function_dependency_set: {cache_key} definition is not indexed by any root"
            ));
        } else if !indexed_dependencies
            .iter()
            .any(|(_, dependencies)| dependencies == &metadata_dependencies)
        {
            errors.push(format!(
                "bad_function_dependency_set: {cache_key} dependency metadata does not match any indexed root"
            ));
        }
        Ok(())
    }

    fn verify_interface_hash_artifact(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let Some(metadata) = artifact_inner_metadata(artifact_json) else {
            errors.push(format!(
                "bad_interface_hash: {cache_key} missing interface metadata"
            ));
            return Ok(());
        };
        if !key_input.dependency_interface_hashes.is_empty()
            || !key_input.dependency_implementation_hashes.is_empty()
        {
            errors.push(format!(
                "bad_interface_hash: {cache_key} cache key should not record dependencies"
            ));
        }
        let recomputed_input = hash_object_canonical(
            "FunctionInterface",
            SCHEMA_VERSION,
            &canonical_json(metadata),
        );
        if recomputed_input != key_input.input_hash {
            errors.push(format!(
                "bad_interface_hash: {cache_key} metadata does not match cache key input"
            ));
        }
        let Some(symbol_hash) = metadata.get("symbol_hash").and_then(JsonValue::as_str) else {
            errors.push(format!(
                "bad_interface_hash: {cache_key} missing symbol_hash"
            ));
            return Ok(());
        };
        let Some(signature_hash) = metadata.get("signature_hash").and_then(JsonValue::as_str)
        else {
            errors.push(format!(
                "bad_interface_hash: {cache_key} missing signature_hash"
            ));
            return Ok(());
        };
        let expected = function_interface_metadata(symbol_hash, signature_hash)?;
        if &expected != metadata {
            errors.push(format!(
                "bad_interface_hash: {cache_key} interface metadata is not canonical for symbol/signature"
            ));
        }
        Ok(())
    }

    fn verify_implementation_hash_artifact(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let Some(metadata) = artifact_inner_metadata(artifact_json) else {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} missing implementation metadata"
            ));
            return Ok(());
        };
        if !key_input.dependency_implementation_hashes.is_empty() {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} cache key should not record implementation dependencies"
            ));
        }
        if metadata.get("definition_hash").and_then(JsonValue::as_str)
            != Some(key_input.input_hash.as_str())
        {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} definition metadata does not match cache key input"
            ));
        }
        let definition = self.get_payload(&key_input.input_hash)?;
        verify_json_field_matches(
            errors,
            "bad_implementation_hash",
            cache_key,
            metadata,
            "symbol_hash",
            &definition,
            "symbol",
        );
        verify_json_field_matches(
            errors,
            "bad_implementation_hash",
            cache_key,
            metadata,
            "function_sig_hash",
            &definition,
            "function_sig_hash",
        );
        verify_json_field_matches(
            errors,
            "bad_implementation_hash",
            cache_key,
            metadata,
            "typed_body_expr_hash",
            &definition,
            "typed_body_expr_hash",
        );
        if metadata
            .get("semantic_lowering_version")
            .and_then(JsonValue::as_str)
            != Some(crate::lowering::LOWERED_IR_SCHEMA)
        {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} semantic lowering version mismatch"
            ));
        }
        if let Some(symbol_hash) = metadata.get("symbol_hash").and_then(JsonValue::as_str) {
            let expected_internal = internal_abi_symbol(symbol_hash)?;
            if metadata
                .get("internal_abi_symbol")
                .and_then(JsonValue::as_str)
                != Some(expected_internal.as_str())
            {
                errors.push(format!(
                    "bad_implementation_hash: {cache_key} internal ABI symbol mismatch"
                ));
            }
        }

        let Some(metadata_dependency_symbols) =
            json_string_vec(metadata.get("direct_dependency_symbols"))
        else {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} direct_dependency_symbols must be a string array"
            ));
            return Ok(());
        };
        let Some(metadata_dependency_interfaces) =
            json_string_vec(metadata.get("direct_dependency_interface_hashes"))
        else {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} direct_dependency_interface_hashes must be a string array"
            ));
            return Ok(());
        };

        if metadata_dependency_interfaces != key_input.dependency_interface_hashes {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} dependency interface metadata does not match cache key"
            ));
        }

        let indexed_roots = self.dependency_symbols_for_indexed_roots(&key_input.input_hash)?;
        if indexed_roots.is_empty() {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} definition is not indexed by any root"
            ));
            return Ok(());
        }
        let matching_root = indexed_roots
            .iter()
            .find(|(_, dependencies)| dependencies == &metadata_dependency_symbols);
        let Some((root_hash, _)) = matching_root else {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} dependency metadata does not match any indexed root"
            ));
            return Ok(());
        };

        let root = self.load_root(root_hash)?;
        let expected_dependency_interfaces =
            self.interface_hashes_for_symbols(&root, &metadata_dependency_symbols)?;
        if metadata_dependency_interfaces != expected_dependency_interfaces {
            errors.push(format!(
                "bad_implementation_hash: {cache_key} dependency interface metadata does not match indexed root"
            ));
        }
        Ok(())
    }

    fn verify_type_layout_artifact(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let Some(metadata) = artifact_inner_metadata(artifact_json) else {
            errors.push(format!(
                "bad_type_layout: {cache_key} missing layout metadata"
            ));
            return Ok(());
        };
        if key_input.backend_id != TYPE_LAYOUT_BACKEND_ID {
            errors.push(format!(
                "bad_type_layout: {cache_key} cache key backend must be {TYPE_LAYOUT_BACKEND_ID}"
            ));
        }
        if !key_input.dependency_interface_hashes.is_empty() {
            errors.push(format!(
                "bad_type_layout: {cache_key} cache key should not record interface dependencies"
            ));
        }
        if metadata.get("schema").and_then(JsonValue::as_str) != Some(TYPE_LAYOUT_SCHEMA) {
            errors.push(format!("bad_type_layout: {cache_key} schema mismatch"));
        }
        if metadata.get("type_hash").and_then(JsonValue::as_str)
            != Some(key_input.input_hash.as_str())
        {
            errors.push(format!("bad_type_layout: {cache_key} type hash mismatch"));
        }
        if metadata.get("target_triple").and_then(JsonValue::as_str)
            != Some(key_input.target_triple.as_str())
        {
            errors.push(format!("bad_type_layout: {cache_key} target mismatch"));
        }
        let Some(metadata_dependencies) = json_string_vec(metadata.get("type_dependency_hashes"))
        else {
            errors.push(format!(
                "bad_type_layout: {cache_key} type_dependency_hashes must be a string array"
            ));
            return Ok(());
        };
        if metadata_dependencies != key_input.dependency_implementation_hashes {
            errors.push(format!(
                "bad_type_layout: {cache_key} dependency metadata does not match cache key"
            ));
        }

        let candidates = self.indexed_program_roots()?;
        if candidates.is_empty() {
            errors.push(format!(
                "bad_type_layout: {cache_key} cannot be recomputed: no indexed roots"
            ));
            return Ok(());
        }
        let mut last_error = None;
        for root_hash in candidates {
            let root = self.load_root(&root_hash)?;
            match self.compute_type_layout(&root, &key_input.input_hash, &key_input.target_triple) {
                Ok(expected) => {
                    let expected_key = type_layout_cache_key_input(
                        &key_input.input_hash,
                        &key_input.target_triple,
                        expected.dependency_type_def_hashes.clone(),
                    );
                    let expected_cache_key = cache_key_for_input(&expected_key)?;
                    if &expected.metadata == metadata
                        && expected.dependency_type_def_hashes
                            == key_input.dependency_implementation_hashes
                        && expected_cache_key == cache_key
                    {
                        return Ok(());
                    }
                }
                Err(err) => last_error = Some(err),
            }
        }
        if let Some(err) = last_error {
            errors.push(format!(
                "bad_type_layout: {cache_key} cannot be recomputed from any indexed root: {err:#}"
            ));
        } else {
            errors.push(format!(
                "bad_type_layout: {cache_key} layout metadata does not match any indexed root"
            ));
        }
        Ok(())
    }

    fn indexed_program_roots(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM objects WHERE kind = 'ProgramRoot' ORDER BY hash")?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn dependency_symbols_for_indexed_roots(
        &self,
        definition_hash: &str,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT root_hash
             FROM root_symbols
             WHERE definition_hash = ?1
             ORDER BY root_hash",
        )?;
        let rows = stmt.query_map([definition_hash], |row| row.get::<_, String>(0))?;
        let mut dependency_sets = Vec::new();
        for row in rows {
            let root_hash = row?;
            let root = self.load_root(&root_hash)?;
            let dependencies = self
                .dependencies_for_definition(&root, definition_hash)?
                .into_iter()
                .collect::<Vec<_>>();
            dependency_sets.push((root_hash, dependencies));
        }
        Ok(dependency_sets)
    }

    fn interface_hashes_for_symbols(
        &self,
        root: &ProgramRootPayload,
        symbols: &[String],
    ) -> Result<Vec<String>> {
        let mut hashes = Vec::new();
        for symbol in symbols {
            let Some(entry) = self.root_symbol(root, symbol) else {
                bail!("dependency symbol is not in indexed root: {symbol}");
            };
            let interface = function_interface_metadata(&entry.symbol, &entry.signature)?;
            hashes.push(hash_bytes(
                BYTES_DOMAIN,
                canonical_json(&interface).as_bytes(),
            ));
        }
        hashes.sort();
        hashes.dedup();
        Ok(hashes)
    }

    fn verify_object_artifact(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        artifact_json: &JsonValue,
        artifact_bytes: Option<&[u8]>,
    ) -> Result<()> {
        let Some(metadata) = artifact_inner_metadata(artifact_json) else {
            errors.push(format!(
                "bad_object_artifact: {cache_key} missing object metadata"
            ));
            return Ok(());
        };
        if metadata.get("schema").and_then(JsonValue::as_str) != Some("codedb/native-object/v1") {
            errors.push(format!(
                "bad_object_artifact: {cache_key} bad object metadata schema"
            ));
        }
        if metadata.get("backend_id").and_then(JsonValue::as_str)
            != Some(key_input.backend_id.as_str())
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} backend metadata mismatch"
            ));
        }
        if metadata.get("target_triple").and_then(JsonValue::as_str)
            != Some(key_input.target_triple.as_str())
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} target metadata mismatch"
            ));
        }
        if metadata
            .get("function_def_hash")
            .and_then(JsonValue::as_str)
            != Some(key_input.input_hash.as_str())
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} function definition metadata mismatch"
            ));
        }
        match self.get_kind(&key_input.input_hash) {
            Ok(kind) if kind == "FunctionDef" => {}
            Ok(kind) => errors.push(format!(
                "bad_object_artifact: {cache_key} input object is {kind}, not FunctionDef"
            )),
            Err(err) => errors.push(format!(
                "bad_object_artifact: {cache_key} cannot load input object kind: {err:#}"
            )),
        }
        match self.get_payload(&key_input.input_hash) {
            Ok(definition) => {
                for (metadata_key, definition_key, label) in [
                    ("symbol_hash", "symbol", "symbol"),
                    ("function_sig_hash", "function_sig_hash", "signature"),
                    ("typed_body_expr_hash", "typed_body_expr_hash", "typed body"),
                ] {
                    if metadata.get(metadata_key).and_then(JsonValue::as_str)
                        != definition.get(definition_key).and_then(JsonValue::as_str)
                    {
                        errors.push(format!(
                            "bad_object_artifact: {cache_key} {label} metadata does not match FunctionDef"
                        ));
                    }
                }
            }
            Err(err) => errors.push(format!(
                "bad_object_artifact: {cache_key} cannot load FunctionDef payload: {err:#}"
            )),
        }
        let symbol = metadata
            .get("symbol_hash")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        if symbol.is_empty() {
            errors.push(format!(
                "bad_object_artifact: {cache_key} missing symbol hash"
            ));
        } else if let Ok(internal_symbol) = internal_abi_symbol(symbol) {
            if !json_array_contains_str(metadata.get("defined_symbols"), &internal_symbol) {
                errors.push(format!(
                    "bad_object_artifact: {cache_key} defined symbols do not include internal ABI symbol"
                ));
            }
        } else {
            errors.push(format!(
                "bad_object_artifact: {cache_key} invalid symbol hash"
            ));
        }
        for field in [
            "defined_symbols",
            "called_symbols",
            "relocations",
            "dependency_interface_hashes",
            "dependency_implementation_hashes",
            "dependency_closure",
        ] {
            if metadata.get(field).and_then(JsonValue::as_array).is_none() {
                errors.push(format!(
                    "bad_object_artifact: {cache_key} malformed metadata: {field} must be an array"
                ));
            }
        }
        if json_string_set(metadata.get("dependency_interface_hashes"))
            != key_input
                .dependency_interface_hashes
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} dependency interface hashes mismatch"
            ));
        }
        if json_string_set(metadata.get("dependency_implementation_hashes"))
            != key_input
                .dependency_implementation_hashes
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} dependency implementation hashes mismatch"
            ));
        }
        if metadata
            .get("dependency_closure")
            .and_then(JsonValue::as_array)
            .is_none()
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} missing dependency closure"
            ));
        }
        for relocation in metadata
            .get("relocations")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            if relocation
                .get("target_symbol_hash")
                .and_then(JsonValue::as_str)
                .is_none()
                || relocation
                    .get("target_abi_symbol")
                    .and_then(JsonValue::as_str)
                    .is_none()
            {
                errors.push(format!(
                    "bad_object_artifact: {cache_key} malformed relocation"
                ));
            }
        }
        let uses_builtin_native_backend =
            object_artifact_uses_builtin_native_backend(key_input, metadata);
        if uses_builtin_native_backend || metadata.get("debug_metadata").is_some() {
            verify_native_debug_metadata_shape(errors, cache_key, metadata);
        }
        if uses_builtin_native_backend {
            self.verify_object_artifact_matches_indexed_root(
                errors,
                cache_key,
                key_input,
                metadata,
                artifact_bytes,
            )?;
        }
        if let Some(bytes) = artifact_bytes {
            if uses_builtin_native_backend {
                verify_native_object_bytes_match_metadata(
                    errors,
                    cache_key,
                    &key_input.target_triple,
                    metadata,
                    bytes,
                );
            } else {
                verify_native_object_bytes_have_declared_format(
                    errors,
                    cache_key,
                    &key_input.target_triple,
                    bytes,
                );
            }
        }
        Ok(())
    }

    fn verify_object_artifact_matches_indexed_root(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        metadata: &JsonValue,
        artifact_bytes: Option<&[u8]>,
    ) -> Result<()> {
        let Some(symbol) = metadata.get("symbol_hash").and_then(JsonValue::as_str) else {
            return Ok(());
        };
        if symbol.is_empty() {
            return Ok(());
        }
        let Some(called_symbols) = json_string_vec(metadata.get("called_symbols")) else {
            return Ok(());
        };
        let Some(metadata_dependency_interfaces) =
            json_string_vec(metadata.get("dependency_interface_hashes"))
        else {
            return Ok(());
        };
        let Some(metadata_dependency_implementations) =
            json_string_vec(metadata.get("dependency_implementation_hashes"))
        else {
            return Ok(());
        };
        let Some(metadata_dependency_closure) = json_string_vec(metadata.get("dependency_closure"))
        else {
            return Ok(());
        };
        let Some(relocations) = metadata.get("relocations").and_then(JsonValue::as_array) else {
            return Ok(());
        };

        let indexed_roots = self.dependency_symbols_for_indexed_roots(&key_input.input_hash)?;
        if indexed_roots.is_empty() {
            errors.push(format!(
                "bad_object_artifact: {cache_key} definition is not indexed by any root"
            ));
            return Ok(());
        }

        let mut saw_dependency_match = false;
        let mut saw_interface_match = false;
        let mut saw_implementation_match = false;
        for (root_hash, dependencies) in indexed_roots {
            if dependencies != called_symbols {
                continue;
            }
            saw_dependency_match = true;
            let root = self.load_root(&root_hash)?;
            let expected_interfaces = self.interface_hashes_for_symbols(&root, &dependencies)?;
            if expected_interfaces.into_iter().collect::<BTreeSet<_>>()
                != metadata_dependency_interfaces
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>()
            {
                continue;
            }
            saw_interface_match = true;
            let Some(entry) = self.root_symbol(&root, symbol) else {
                errors.push(format!(
                    "bad_object_artifact: {cache_key} symbol missing from indexed root {root_hash}"
                ));
                return Ok(());
            };
            let expected_ir =
                self.build_lowered_function_ir(&root, entry, &key_input.target_triple)?;
            let expected_implementation_dependencies = self.lowered_ir_type_dependency_hashes(
                &root,
                &expected_ir,
                &key_input.target_triple,
            )?;
            if expected_implementation_dependencies != metadata_dependency_implementations {
                continue;
            }
            saw_implementation_match = true;
            let expected_closure = self.verify_dependency_closure_for_symbol(&root_hash, symbol)?;
            if expected_closure != metadata_dependency_closure {
                continue;
            }
            let expected_relocation_targets = lowered_call_targets(&expected_ir)?;
            verify_object_relocations_match_dependencies(
                errors,
                cache_key,
                relocations,
                &dependencies,
                &expected_relocation_targets,
            );
            verify_object_debug_metadata_matches_lowered_ir(
                errors,
                cache_key,
                metadata,
                &expected_ir,
            );
            if let Some(bytes) = artifact_bytes {
                verify_builtin_native_object_bytes_reemit(
                    errors,
                    cache_key,
                    key_input,
                    &expected_ir,
                    bytes,
                    metadata,
                );
            }
            return Ok(());
        }

        if !saw_dependency_match {
            errors.push(format!(
                "bad_object_artifact: {cache_key} called_symbols metadata does not match any indexed root"
            ));
        } else if !saw_interface_match {
            errors.push(format!(
                "bad_object_artifact: {cache_key} dependency interface metadata does not match indexed root"
            ));
        } else if !saw_implementation_match {
            errors.push(format!(
                "bad_object_artifact: {cache_key} dependency implementation metadata does not match indexed root"
            ));
        } else {
            errors.push(format!(
                "bad_object_artifact: {cache_key} dependency closure metadata does not match indexed root"
            ));
        }
        Ok(())
    }

    fn lowered_ir_type_dependency_hashes(
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

    fn verify_dependency_closure_for_symbol(
        &self,
        root_hash: &str,
        origin: &str,
    ) -> Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        self.collect_verify_dependency_closure(root_hash, origin, origin, &mut seen)?;
        Ok(seen.into_iter().collect())
    }

    fn collect_verify_dependency_closure(
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
                self.collect_verify_dependency_closure(root_hash, origin, &dependency, seen)?;
            }
        }
        Ok(())
    }

    fn verify_link_plan_artifact(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let Some(plan) = artifact_inner_metadata(artifact_json) else {
            errors.push(format!("bad_link_plan: {cache_key} missing plan metadata"));
            return Ok(());
        };
        if plan.get("schema").and_then(JsonValue::as_str) != Some("codedb/link-plan/v1") {
            errors.push(format!("bad_link_plan: {cache_key} bad schema"));
        }
        if plan.get("input_hash").and_then(JsonValue::as_str) != Some(key_input.input_hash.as_str())
        {
            errors.push(format!("bad_link_plan: {cache_key} input hash mismatch"));
        }
        if plan.get("target_triple").and_then(JsonValue::as_str)
            != Some(key_input.target_triple.as_str())
        {
            errors.push(format!("bad_link_plan: {cache_key} target mismatch"));
        }
        if plan.get("output_kind").and_then(JsonValue::as_str) != Some("executable") {
            errors.push(format!(
                "bad_link_plan: {cache_key} missing or unsupported output kind"
            ));
        }
        let object_symbols = plan
            .get("objects")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(|object| object.get("symbol_hash").and_then(JsonValue::as_str))
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let external_symbols = link_plan_external_symbols(plan);
        let linked_symbols = object_symbols
            .iter()
            .cloned()
            .chain(external_symbols.iter().cloned())
            .collect::<BTreeSet<_>>();
        if let Some(entry_symbol) = plan.get("entry_symbol_hash").and_then(JsonValue::as_str) {
            if !linked_symbols.contains(entry_symbol) {
                errors.push(format!(
                    "bad_link_plan: {cache_key} entry symbol is not linked"
                ));
            }
        } else {
            errors.push(format!("bad_link_plan: {cache_key} missing entry symbol"));
        }
        for object in plan
            .get("objects")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            let Some(symbol) = object.get("symbol_hash").and_then(JsonValue::as_str) else {
                errors.push(format!("bad_link_plan: {cache_key} object missing symbol"));
                continue;
            };
            match internal_abi_symbol(symbol) {
                Ok(expected) => {
                    if object
                        .get("internal_abi_symbol")
                        .and_then(JsonValue::as_str)
                        != Some(expected.as_str())
                    {
                        errors.push(format!(
                            "bad_link_plan: {cache_key} object internal ABI symbol mismatch"
                        ));
                    }
                }
                Err(err) => errors.push(format!(
                    "bad_link_plan: {cache_key} object has invalid symbol hash: {err:#}"
                )),
            }
            let object_cache_key = object.get("object_cache_key").and_then(JsonValue::as_str);
            let object_artifact_hash = object
                .get("object_artifact_hash")
                .and_then(JsonValue::as_str);
            match (object_cache_key, object_artifact_hash) {
                (Some(object_cache_key), Some(object_artifact_hash)) => {
                    match self
                        .object_artifact_metadata_for_key(object_cache_key, object_artifact_hash)
                    {
                        Ok(Some(object_metadata)) => {
                            verify_link_plan_object_matches_object_metadata(
                                errors,
                                cache_key,
                                object,
                                &object_metadata,
                            );
                        }
                        Ok(None) => {
                            errors.push(format!(
                                "bad_link_plan: {cache_key} object cache key does not identify artifact {object_artifact_hash}"
                            ));
                        }
                        Err(err) => {
                            errors.push(format!(
                                "bad_link_plan: {cache_key} cannot read object artifact metadata: {err:#}"
                            ));
                        }
                    }
                }
                (None, _) => errors.push(format!(
                    "bad_link_plan: {cache_key} object missing object cache key"
                )),
                (_, None) => errors.push(format!(
                    "bad_link_plan: {cache_key} object missing object artifact hash"
                )),
            }
        }
        for external in plan
            .get("external_symbols")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            let Some(symbol) = external.get("symbol_hash").and_then(JsonValue::as_str) else {
                errors.push(format!(
                    "bad_link_plan: {cache_key} external symbol missing symbol"
                ));
                continue;
            };
            if object_symbols.contains(symbol) {
                errors.push(format!(
                    "bad_link_plan: {cache_key} external symbol is also backed by an object"
                ));
            }
            if let Some(abi) = external.get("abi").and_then(JsonValue::as_str) {
                if let Err(err) = validate_external_abi_tag(abi) {
                    errors.push(format!(
                        "bad_link_plan: {cache_key} invalid external ABI: {err:#}"
                    ));
                }
            } else {
                errors.push(format!(
                    "bad_link_plan: {cache_key} external symbol missing ABI"
                ));
            }
            if let Some(link_name) = external.get("link_name").and_then(JsonValue::as_str) {
                if let Err(err) = validate_external_link_name(link_name) {
                    errors.push(format!(
                        "bad_link_plan: {cache_key} invalid external link name: {err:#}"
                    ));
                }
            } else {
                errors.push(format!(
                    "bad_link_plan: {cache_key} external symbol missing link_name"
                ));
            }
            if let Some(library) = external.get("library").and_then(JsonValue::as_str)
                && let Err(err) = validate_external_library_name(library)
            {
                errors.push(format!(
                    "bad_link_plan: {cache_key} invalid external library: {err:#}"
                ));
            }
        }
        let object_hashes = plan
            .get("objects")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(|object| {
                object
                    .get("object_artifact_hash")
                    .and_then(JsonValue::as_str)
            })
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let object_cache_keys = plan
            .get("objects")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(|object| object.get("object_cache_key").and_then(JsonValue::as_str))
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        if object_cache_keys
            != key_input
                .dependency_implementation_hashes
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
        {
            errors.push(format!(
                "bad_link_plan: {cache_key} object cache key dependencies mismatch"
            ));
        }
        for object_cache_key in &object_cache_keys {
            if !self.cache_key_exists(ArtifactKind::ObjectFile, object_cache_key)? {
                errors.push(format!(
                    "bad_link_plan: {cache_key} references missing object cache key {object_cache_key}"
                ));
            }
        }
        for object_hash in &object_hashes {
            if !self.cache_artifact_exists(ArtifactKind::ObjectFile, object_hash)? {
                errors.push(format!(
                    "bad_link_plan: {cache_key} references missing object artifact {object_hash}"
                ));
            }
        }
        match self.get_payload(&key_input.input_hash) {
            Ok(input) => {
                if input.get("schema").and_then(JsonValue::as_str) != Some("codedb/link-input/v1") {
                    errors.push(format!("bad_link_plan: {cache_key} bad link input schema"));
                }
                if input.get("target_triple") != plan.get("target_triple")
                    || input.get("entry_symbol_hash") != plan.get("entry_symbol_hash")
                    || input.get("entry_abi_symbol") != plan.get("entry_abi_symbol")
                    || input.get("external_symbols") != plan.get("external_symbols")
                    || input.get("export_map") != plan.get("export_map")
                    || input.get("output_kind") != plan.get("output_kind")
                    || input.get("link_options") != plan.get("link_options")
                {
                    errors.push(format!(
                        "bad_link_plan: {cache_key} plan does not match link input"
                    ));
                }
                if input.get("object_artifact_hashes").is_some()
                    && json_string_set(input.get("object_artifact_hashes")) != object_hashes
                {
                    errors.push(format!(
                        "bad_link_plan: {cache_key} object list does not match link input"
                    ));
                }
                if json_string_set(input.get("object_cache_keys")) != object_cache_keys {
                    errors.push(format!(
                        "bad_link_plan: {cache_key} object cache key list does not match link input"
                    ));
                }
            }
            Err(err) => errors.push(format!(
                "bad_link_plan: {cache_key} cannot load link input: {err:#}"
            )),
        }
        for export in plan
            .get("export_map")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            let symbol = export.get("symbol_hash").and_then(JsonValue::as_str);
            let internal_symbol = export
                .get("internal_abi_symbol")
                .and_then(JsonValue::as_str);
            let exported_symbol = export
                .get("exported_abi_symbol")
                .and_then(JsonValue::as_str);
            if exported_symbol.is_none_or(|name| validate_exported_abi_name(name).is_err()) {
                errors.push(format!("bad_link_plan: {cache_key} invalid export map"));
            }
            let Some(symbol) = symbol else {
                errors.push(format!("bad_link_plan: {cache_key} export missing symbol"));
                continue;
            };
            if !object_symbols.contains(symbol) {
                errors.push(format!(
                    "bad_link_plan: {cache_key} export is not backed by a linked object"
                ));
            }
            match internal_abi_symbol(symbol) {
                Ok(expected) => {
                    if internal_symbol != Some(expected.as_str()) {
                        errors.push(format!(
                            "bad_link_plan: {cache_key} export internal ABI symbol mismatch"
                        ));
                    }
                }
                Err(err) => errors.push(format!(
                    "bad_link_plan: {cache_key} export has invalid symbol hash: {err:#}"
                )),
            }
        }
        self.verify_link_plan_recomputes_from_indexed_root(errors, cache_key, plan)?;
        Ok(())
    }

    fn verify_link_plan_recomputes_from_indexed_root(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        plan: &JsonValue,
    ) -> Result<()> {
        let Some(entry_symbol) = plan.get("entry_symbol_hash").and_then(JsonValue::as_str) else {
            return Ok(());
        };
        let candidates = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT root_hash FROM root_symbols
                 WHERE symbol_hash = ?1 ORDER BY root_hash",
            )?;
            stmt.query_map(params![entry_symbol], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if candidates.is_empty() {
            errors.push(format!(
                "bad_link_plan: {cache_key} cannot be recomputed: entry symbol is in no indexed root"
            ));
            return Ok(());
        }

        let mut last_error = None;
        for root_hash in candidates {
            match self.link_plan_matches_indexed_root(&root_hash, plan) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(err) => last_error = Some(err),
            }
        }
        if let Some(err) = last_error {
            errors.push(format!(
                "bad_link_plan: {cache_key} cannot be recomputed from any indexed root: {err:#}"
            ));
        } else {
            errors.push(format!(
                "bad_link_plan: {cache_key} cannot be recomputed from any indexed root"
            ));
        }
        Ok(())
    }

    fn link_plan_matches_indexed_root(&self, root_hash: &str, plan: &JsonValue) -> Result<bool> {
        let Some(entry_symbol) = plan.get("entry_symbol_hash").and_then(JsonValue::as_str) else {
            return Ok(false);
        };
        let Some(entry_abi_symbol) = plan.get("entry_abi_symbol").and_then(JsonValue::as_str)
        else {
            return Ok(false);
        };
        let root = self.load_root(root_hash)?;
        let Some(entry) = self.root_symbol(&root, entry_symbol) else {
            return Ok(false);
        };
        let expected_entry_abi = if self.definition_is_external(&entry.definition)? {
            self.external_function_metadata(&entry.definition)?
                .link_name
        } else {
            internal_abi_symbol(entry_symbol)?
        };
        if entry_abi_symbol != expected_entry_abi {
            return Ok(false);
        }
        let planned_symbols = link_plan_object_symbols(plan);
        let planned_external_symbols = link_plan_external_symbols(plan);
        let all_planned_symbols = planned_symbols
            .iter()
            .cloned()
            .chain(planned_external_symbols.iter().cloned())
            .collect::<BTreeSet<_>>();
        if self
            .reachable_symbols(root_hash, entry_symbol)?
            .into_iter()
            .collect::<BTreeSet<_>>()
            != all_planned_symbols
        {
            return Ok(false);
        }

        for object in plan
            .get("objects")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            let Some(symbol) = object.get("symbol_hash").and_then(JsonValue::as_str) else {
                return Ok(false);
            };
            let Some(entry) = self.root_symbol(&root, symbol) else {
                return Ok(false);
            };
            if self.definition_is_external(&entry.definition)? {
                return Ok(false);
            }
            let expected_internal_abi = internal_abi_symbol(symbol)?;
            if object.get("definition_hash").and_then(JsonValue::as_str)
                != Some(entry.definition.as_str())
                || object.get("signature_hash").and_then(JsonValue::as_str)
                    != Some(entry.signature.as_str())
                || object
                    .get("internal_abi_symbol")
                    .and_then(JsonValue::as_str)
                    != Some(expected_internal_abi.as_str())
            {
                return Ok(false);
            }
            let (param_types, return_type) = self.signature_parts(&entry.signature)?;
            if json_string_vec(object.get("param_type_hashes")) != Some(param_types.clone())
                || object.get("return_type_hash").and_then(JsonValue::as_str)
                    != Some(return_type.as_str())
            {
                return Ok(false);
            }
        }
        let expected_external_symbols = planned_external_symbols
            .iter()
            .map(|symbol| {
                let entry = self
                    .root_symbol(&root, symbol)
                    .ok_or_else(|| anyhow!("external symbol missing from root {symbol}"))?;
                let metadata = self.external_function_metadata(&entry.definition)?;
                let (param_type_hashes, return_type_hash) =
                    self.signature_parts(&entry.signature)?;
                Ok(json!({
                    "symbol_hash": symbol,
                    "definition_hash": &entry.definition,
                    "signature_hash": &entry.signature,
                    "param_type_hashes": param_type_hashes,
                    "return_type_hash": return_type_hash,
                    "effects": self.signature_effect_names(&entry.signature)?,
                    "abi": metadata.abi,
                    "link_name": metadata.link_name,
                    "library": metadata.library,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        if json_value_set(plan.get("external_symbols"))
            != json_value_set(Some(&json!(expected_external_symbols)))
        {
            return Ok(false);
        }

        let linked_symbols = planned_symbols.into_iter().collect::<BTreeSet<_>>();
        let expected_exports = export_map(&root)?
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
        Ok(plan.get("export_map") == Some(&json!(expected_exports)))
    }

    fn verify_executable_artifact(
        &self,
        errors: &mut Vec<String>,
        cache_key: &str,
        key_input: &CacheKeyInput,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let Some(metadata) = artifact_inner_metadata(artifact_json) else {
            errors.push(format!(
                "bad_executable_artifact: {cache_key} missing executable metadata"
            ));
            return Ok(());
        };
        if metadata.get("schema").and_then(JsonValue::as_str) != Some("codedb/executable/v1") {
            errors.push(format!(
                "bad_executable_artifact: {cache_key} bad executable metadata schema"
            ));
        }
        if metadata.get("target_triple").and_then(JsonValue::as_str)
            != Some(key_input.target_triple.as_str())
        {
            errors.push(format!(
                "bad_executable_artifact: {cache_key} target mismatch"
            ));
        }
        let dependency_hashes = key_input
            .dependency_implementation_hashes
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let link_plan_hash = metadata
            .get("link_plan_hash")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        if !dependency_hashes.contains(link_plan_hash)
            || !self.cache_artifact_exists(ArtifactKind::LinkPlan, link_plan_hash)?
        {
            errors.push(format!(
                "bad_executable_artifact: {cache_key} missing link plan dependency"
            ));
        }
        let linker_identity_hash = metadata
            .get("linker_identity_hash")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        if !dependency_hashes.contains(linker_identity_hash) {
            errors.push(format!(
                "bad_executable_artifact: {cache_key} missing linker identity dependency"
            ));
        }
        let object_cache_keys = json_string_set(metadata.get("object_cache_keys"));
        let mut expected_object_dependencies = dependency_hashes.clone();
        expected_object_dependencies.remove(link_plan_hash);
        expected_object_dependencies.remove(linker_identity_hash);
        if object_cache_keys != expected_object_dependencies {
            errors.push(format!(
                "bad_executable_artifact: {cache_key} object cache key dependencies mismatch"
            ));
        }
        for object_cache_key in object_cache_keys {
            if !dependency_hashes.contains(&object_cache_key)
                || !self.cache_key_exists(ArtifactKind::ObjectFile, &object_cache_key)?
            {
                errors.push(format!(
                    "bad_executable_artifact: {cache_key} missing object dependency {object_cache_key}"
                ));
            }
        }
        for object_hash in json_string_set(metadata.get("object_artifact_hashes")) {
            if !self.cache_artifact_exists(ArtifactKind::ObjectFile, &object_hash)? {
                errors.push(format!(
                    "bad_executable_artifact: {cache_key} references missing object artifact {object_hash}"
                ));
            }
        }
        Ok(())
    }

    fn cache_artifact_exists(
        &self,
        artifact_kind: ArtifactKind,
        artifact_hash: &str,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM compile_cache
                WHERE artifact_kind = ?1 AND artifact_hash = ?2
             )",
            params![artifact_kind.as_str(), artifact_hash],
            |row| row.get(0),
        )?)
    }

    fn cache_key_exists(&self, artifact_kind: ArtifactKind, cache_key: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM compile_cache
                WHERE artifact_kind = ?1 AND cache_key = ?2
             )",
            params![artifact_kind.as_str(), cache_key],
            |row| row.get(0),
        )?)
    }

    fn object_artifact_metadata_for_key(
        &self,
        cache_key: &str,
        artifact_hash: &str,
    ) -> Result<Option<JsonValue>> {
        let artifact_json: Option<String> = self
            .conn
            .query_row(
                "SELECT artifact_json
                 FROM compile_cache
                 WHERE artifact_kind = ?1 AND cache_key = ?2 AND artifact_hash = ?3",
                params![ArtifactKind::ObjectFile.as_str(), cache_key, artifact_hash],
                |row| row.get(0),
            )
            .optional()?;
        artifact_json
            .map(|artifact_json| {
                let value = serde_json::from_str::<JsonValue>(&artifact_json)?;
                artifact_inner_metadata(&value)
                    .cloned()
                    .ok_or_else(|| anyhow!("object artifact missing metadata"))
            })
            .transpose()
    }
}

struct ArtifactMetadataCheck<'a> {
    cache_key: &'a str,
    artifact_kind: ArtifactKind,
    input_hash: &'a str,
    backend: &'a str,
    target: &'a str,
    artifact_hash: &'a str,
    artifact_json: &'a JsonValue,
    artifact_bytes: Option<&'a [u8]>,
}

fn verify_artifact_metadata(errors: &mut Vec<String>, check: ArtifactMetadataCheck<'_>) {
    if check
        .artifact_json
        .get("schema")
        .and_then(JsonValue::as_str)
        != Some(ARTIFACT_METADATA_SCHEMA)
    {
        errors.push(format!(
            "bad_cache_entry: artifact metadata schema mismatch {}",
            check.cache_key
        ));
        return;
    }
    if check
        .artifact_json
        .get("artifact_kind")
        .and_then(JsonValue::as_str)
        != Some(check.artifact_kind.as_str())
    {
        errors.push(format!(
            "bad_cache_entry: artifact metadata kind mismatch {}",
            check.cache_key
        ));
    }
    if check
        .artifact_json
        .get("input_hash")
        .and_then(JsonValue::as_str)
        != Some(check.input_hash)
    {
        errors.push(format!(
            "bad_cache_entry: artifact metadata input mismatch {}",
            check.cache_key
        ));
    }
    if check
        .artifact_json
        .get("backend_id")
        .and_then(JsonValue::as_str)
        != Some(check.backend)
    {
        errors.push(format!(
            "bad_cache_entry: artifact metadata backend mismatch {}",
            check.cache_key
        ));
    }
    if check
        .artifact_json
        .get("target_triple")
        .and_then(JsonValue::as_str)
        != Some(check.target)
    {
        errors.push(format!(
            "bad_cache_entry: artifact metadata target mismatch {}",
            check.cache_key
        ));
    }

    match check
        .artifact_json
        .get("content_kind")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
    {
        "text" => {
            let Some(text) = check.artifact_json.get("text").and_then(JsonValue::as_str) else {
                errors.push(format!(
                    "bad_cache_entry: text artifact missing text {}",
                    check.cache_key
                ));
                return;
            };
            let recomputed = hash_bytes(BYTES_DOMAIN, text.as_bytes());
            if recomputed != check.artifact_hash {
                errors.push(format!(
                    "bad_cache_entry: artifact text hash {} recomputes to {recomputed}",
                    check.artifact_hash
                ));
            }
            if check
                .artifact_json
                .get("text_hash")
                .and_then(JsonValue::as_str)
                != Some(check.artifact_hash)
            {
                errors.push(format!(
                    "bad_cache_entry: text artifact metadata hash mismatch {}",
                    check.cache_key
                ));
            }
        }
        "json" => {
            let Some(metadata) = check.artifact_json.get("metadata") else {
                errors.push(format!(
                    "bad_cache_entry: json artifact missing metadata {}",
                    check.cache_key
                ));
                return;
            };
            let recomputed = hash_bytes(BYTES_DOMAIN, canonical_json(metadata).as_bytes());
            if recomputed != check.artifact_hash {
                errors.push(format!(
                    "bad_cache_entry: artifact json hash {} recomputes to {recomputed}",
                    check.artifact_hash
                ));
            }
            if check
                .artifact_json
                .get("metadata_hash")
                .and_then(JsonValue::as_str)
                != Some(check.artifact_hash)
            {
                errors.push(format!(
                    "bad_cache_entry: json artifact metadata hash mismatch {}",
                    check.cache_key
                ));
            }
        }
        "bytes" => {
            let Some(bytes) = check.artifact_bytes else {
                errors.push(format!(
                    "bad_artifact_bytes: bytes artifact missing artifact_bytes {}",
                    check.cache_key
                ));
                return;
            };
            let recomputed = hash_bytes(BYTES_DOMAIN, bytes);
            if recomputed != check.artifact_hash {
                errors.push(format!(
                    "bad_artifact_bytes: artifact bytes hash {} recomputes to {recomputed}",
                    check.artifact_hash
                ));
            }
            if check
                .artifact_json
                .get("bytes_hash")
                .and_then(JsonValue::as_str)
                != Some(check.artifact_hash)
            {
                errors.push(format!(
                    "bad_artifact_bytes: bytes artifact metadata hash mismatch {}",
                    check.cache_key
                ));
            }
        }
        other => errors.push(format!(
            "bad_cache_entry: unknown artifact content kind {other:?} for {}",
            check.cache_key
        )),
    }
}

fn verify_object_relocations_match_dependencies(
    errors: &mut Vec<String>,
    cache_key: &str,
    relocations: &[JsonValue],
    dependency_symbols: &[String],
    expected_relocation_targets: &[ExpectedRelocationTarget],
) {
    let expected_symbols = dependency_symbols.iter().cloned().collect::<BTreeSet<_>>();
    let actual_symbols = relocations
        .iter()
        .filter_map(|relocation| {
            relocation
                .get("target_symbol_hash")
                .and_then(JsonValue::as_str)
        })
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if actual_symbols != expected_symbols {
        errors.push(format!(
            "bad_object_artifact: {cache_key} relocations do not match direct dependencies"
        ));
    }
    let actual_target_counts = string_counts(relocations.iter().filter_map(|relocation| {
        relocation
            .get("target_symbol_hash")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
    }));
    let expected_target_counts = string_counts(
        expected_relocation_targets
            .iter()
            .map(|target| target.symbol_hash.clone()),
    );
    if actual_target_counts != expected_target_counts {
        errors.push(format!(
            "bad_object_artifact: {cache_key} relocations do not match lowered call sites"
        ));
    }
    let expected_abi_by_symbol = expected_relocation_targets
        .iter()
        .map(|target| (target.symbol_hash.clone(), target.abi_symbol.clone()))
        .collect::<BTreeMap<_, _>>();

    for relocation in relocations {
        let Some(target_symbol) = relocation
            .get("target_symbol_hash")
            .and_then(JsonValue::as_str)
        else {
            continue;
        };
        let Some(expected_abi_symbol) = expected_abi_by_symbol.get(target_symbol) else {
            errors.push(format!(
                "bad_object_artifact: {cache_key} relocation target is not a lowered call site"
            ));
            continue;
        };
        if relocation
            .get("target_abi_symbol")
            .and_then(JsonValue::as_str)
            != Some(expected_abi_symbol.as_str())
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} relocation ABI symbol mismatch"
            ));
        }
        if let Some(object_symbol) = relocation
            .get("target_object_symbol")
            .and_then(JsonValue::as_str)
        {
            let expected_object_symbol = format!("_{expected_abi_symbol}");
            if object_symbol != expected_object_symbol {
                errors.push(format!(
                    "bad_object_artifact: {cache_key} relocation object symbol mismatch"
                ));
            }
        }
    }
}

fn verify_native_debug_metadata_shape(
    errors: &mut Vec<String>,
    cache_key: &str,
    metadata: &JsonValue,
) {
    let Some(debug_metadata) = metadata.get("debug_metadata") else {
        errors.push(format!(
            "bad_object_artifact: {cache_key} missing debug metadata"
        ));
        return;
    };
    if debug_metadata.get("schema").and_then(JsonValue::as_str)
        != Some("codedb/native-debug-metadata/v1")
    {
        errors.push(format!(
            "bad_object_artifact: {cache_key} bad debug metadata schema"
        ));
    }
    if debug_metadata
        .get("text_section")
        .and_then(JsonValue::as_str)
        != Some(".text")
    {
        errors.push(format!(
            "bad_object_artifact: {cache_key} debug metadata text section mismatch"
        ));
    }
    let Some(text_size) = debug_metadata.get("text_size").and_then(JsonValue::as_u64) else {
        errors.push(format!(
            "bad_object_artifact: {cache_key} debug metadata missing text_size"
        ));
        return;
    };
    let Some(ranges) = debug_metadata.get("ranges").and_then(JsonValue::as_array) else {
        errors.push(format!(
            "bad_object_artifact: {cache_key} debug metadata ranges must be an array"
        ));
        return;
    };
    if ranges.is_empty() {
        errors.push(format!(
            "bad_object_artifact: {cache_key} debug metadata has no ranges"
        ));
    }
    let symbol_hash = metadata.get("symbol_hash").and_then(JsonValue::as_str);
    let function_def_hash = metadata
        .get("function_def_hash")
        .and_then(JsonValue::as_str);
    for range in ranges {
        let value_id = range.get("value_id").and_then(JsonValue::as_str);
        let lowered_op_id = range.get("lowered_op_id").and_then(JsonValue::as_str);
        match (value_id, lowered_op_id) {
            (Some(value_id), Some(lowered_op_id)) => {
                if lowered_op_id != lowered_op_id_for_value(value_id) {
                    errors.push(format!(
                        "bad_object_artifact: {cache_key} debug range op id does not match value id"
                    ));
                }
            }
            _ => errors.push(format!(
                "bad_object_artifact: {cache_key} malformed debug range identity"
            )),
        }
        if range
            .get("lowered_op_kind")
            .and_then(JsonValue::as_str)
            .is_none()
            || range.get("expr_hash").and_then(JsonValue::as_str).is_none()
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} malformed debug range semantic identity"
            ));
        }
        if range.get("symbol_hash").and_then(JsonValue::as_str) != symbol_hash {
            errors.push(format!(
                "bad_object_artifact: {cache_key} debug range symbol mismatch"
            ));
        }
        if range.get("function_def_hash").and_then(JsonValue::as_str) != function_def_hash {
            errors.push(format!(
                "bad_object_artifact: {cache_key} debug range function definition mismatch"
            ));
        }
        let start = range
            .get("text_offset_start")
            .and_then(JsonValue::as_u64)
            .unwrap_or(u64::MAX);
        let end = range
            .get("text_offset_end")
            .and_then(JsonValue::as_u64)
            .unwrap_or(u64::MAX);
        if start >= end || end > text_size {
            errors.push(format!(
                "bad_object_artifact: {cache_key} malformed debug text range"
            ));
        }
    }
}

fn verify_object_debug_metadata_matches_lowered_ir(
    errors: &mut Vec<String>,
    cache_key: &str,
    metadata: &JsonValue,
    ir: &LoweredFunctionIr,
) {
    let Some(debug_metadata) = metadata.get("debug_metadata") else {
        return;
    };
    let Some(ranges) = debug_metadata.get("ranges").and_then(JsonValue::as_array) else {
        return;
    };
    let expected_debug_ops = match lowered_value_debug_ops(ir) {
        Ok(ops) => ops,
        Err(err) => {
            errors.push(format!(
                "bad_object_artifact: {cache_key} cannot read lowered debug map: {err:#}"
            ));
            return;
        }
    };
    let range_value_ids = ranges
        .iter()
        .filter_map(|range| range.get("value_id").and_then(JsonValue::as_str))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let expected_value_ids = expected_debug_ops.keys().cloned().collect::<BTreeSet<_>>();
    if range_value_ids != expected_value_ids {
        errors.push(format!(
            "bad_object_artifact: {cache_key} debug ranges do not cover lowered operations"
        ));
        return;
    }
    let mut seen_values = BTreeSet::new();
    for range in ranges {
        let Some(value_id) = range.get("value_id").and_then(JsonValue::as_str) else {
            continue;
        };
        if !seen_values.insert(value_id.to_string()) {
            errors.push(format!(
                "bad_object_artifact: {cache_key} duplicate debug range for lowered value"
            ));
            continue;
        }
        let Some(expected) = expected_debug_ops.get(value_id) else {
            continue;
        };
        if range.get("symbol_hash").and_then(JsonValue::as_str) != Some(ir.symbol_hash.as_str())
            || range.get("function_def_hash").and_then(JsonValue::as_str)
                != Some(ir.function_def_hash.as_str())
            || range.get("lowered_op_id").and_then(JsonValue::as_str)
                != Some(expected.lowered_op_id.as_str())
            || range.get("lowered_op_kind").and_then(JsonValue::as_str)
                != Some(expected.lowered_op_kind.as_str())
            || range.get("expr_hash").and_then(JsonValue::as_str)
                != Some(expected.expr_hash.as_str())
        {
            errors.push(format!(
                "bad_object_artifact: {cache_key} debug range does not match lowered IR"
            ));
        }
    }
}

#[derive(Debug, Clone)]
struct ExpectedRelocationTarget {
    symbol_hash: String,
    abi_symbol: String,
}

fn lowered_call_targets(ir: &LoweredFunctionIr) -> Result<Vec<ExpectedRelocationTarget>> {
    let mut targets = Vec::new();
    collect_lowered_call_targets(&ir.operations, &mut targets)?;
    Ok(targets)
}

fn collect_lowered_call_targets(
    operations: &[LoweredOp],
    targets: &mut Vec<ExpectedRelocationTarget>,
) -> Result<()> {
    for op in operations {
        match op {
            LoweredOp::Call {
                target_symbol_hash, ..
            } => {
                let abi_symbol = match op {
                    LoweredOp::Call {
                        target_abi_symbol, ..
                    } => target_abi_symbol
                        .clone()
                        .unwrap_or(internal_abi_symbol(target_symbol_hash)?),
                    _ => unreachable!(),
                };
                targets.push(ExpectedRelocationTarget {
                    symbol_hash: target_symbol_hash.clone(),
                    abi_symbol,
                });
            }
            LoweredOp::If {
                then_block,
                else_block,
                ..
            } => {
                collect_lowered_call_targets(&then_block.operations, targets)?;
                collect_lowered_call_targets(&else_block.operations, targets)?;
            }
            LoweredOp::Case { arms, .. } => {
                for arm in arms {
                    collect_lowered_call_targets(&arm.block.operations, targets)?;
                }
            }
            LoweredOp::Param { .. }
            | LoweredOp::ConstI64 { .. }
            | LoweredOp::ConstBool { .. }
            | LoweredOp::ConstUnit { .. }
            | LoweredOp::Unary { .. }
            | LoweredOp::Binary { .. }
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
            | LoweredOp::BorrowShared { .. }
            | LoweredOp::BorrowMut { .. }
            | LoweredOp::DerefShared { .. }
            | LoweredOp::DerefMut { .. }
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
    Ok(())
}

fn string_counts(values: impl IntoIterator<Item = String>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    counts
}

fn verify_native_object_bytes_match_metadata(
    errors: &mut Vec<String>,
    cache_key: &str,
    target_triple: &str,
    metadata: &JsonValue,
    bytes: &[u8],
) {
    match target_triple {
        crate::LINUX_X86_64_TARGET => verify_elf_object_header(errors, cache_key, bytes),
        crate::APPLE_ARM64_TARGET => verify_macho_object_header(errors, cache_key, bytes),
        _ => {}
    }

    for symbol in required_object_byte_symbols(target_triple, metadata) {
        if !bytes_contain(bytes, symbol.as_bytes()) {
            errors.push(format!(
                "bad_object_artifact: {cache_key} object bytes missing symbol {symbol}"
            ));
        }
    }
}

fn verify_builtin_native_object_bytes_reemit(
    errors: &mut Vec<String>,
    cache_key: &str,
    key_input: &CacheKeyInput,
    ir: &LoweredFunctionIr,
    bytes: &[u8],
    metadata: &JsonValue,
) {
    let emitted = match key_input.backend_id.as_str() {
        ELF_BACKEND_ID => ElfObjectBackend.emit_object(ObjectBackendInput {
            ir,
            target_triple: &key_input.target_triple,
        }),
        MACHO_BACKEND_ID => MachOArm64ObjectBackend.emit_object(ObjectBackendInput {
            ir,
            target_triple: &key_input.target_triple,
        }),
        _ => return,
    };
    match emitted {
        Ok(emitted) => {
            if emitted.bytes != bytes {
                errors.push(format!(
                    "bad_object_artifact: {cache_key} object bytes do not match deterministic native backend output"
                ));
            }
            if emitted.metadata.get("debug_metadata") != metadata.get("debug_metadata") {
                errors.push(format!(
                    "bad_object_artifact: {cache_key} debug metadata does not match deterministic native backend output"
                ));
            }
        }
        Err(err) => errors.push(format!(
            "bad_object_artifact: {cache_key} cannot re-emit native object: {err:#}"
        )),
    }
}

fn verify_native_object_bytes_have_declared_format(
    errors: &mut Vec<String>,
    cache_key: &str,
    target_triple: &str,
    bytes: &[u8],
) {
    match target_triple {
        crate::LINUX_X86_64_TARGET if !bytes.starts_with(b"\x7fELF") => errors.push(format!(
            "bad_object_artifact: {cache_key} object bytes are not ELF"
        )),
        crate::APPLE_ARM64_TARGET if !bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) => {
            errors.push(format!(
                "bad_object_artifact: {cache_key} object bytes are not Mach-O"
            ));
        }
        _ => {}
    }
}

fn object_artifact_uses_builtin_native_backend(
    key_input: &CacheKeyInput,
    metadata: &JsonValue,
) -> bool {
    let backend_id = metadata.get("backend_id").and_then(JsonValue::as_str);
    matches!(
        (key_input.backend_id.as_str(), backend_id),
        (ELF_BACKEND_ID, Some(ELF_BACKEND_ID)) | (MACHO_BACKEND_ID, Some(MACHO_BACKEND_ID))
    )
}

fn verify_elf_object_header(errors: &mut Vec<String>, cache_key: &str, bytes: &[u8]) {
    if bytes.len() < 64 || !bytes.starts_with(b"\x7fELF") {
        errors.push(format!(
            "bad_object_artifact: {cache_key} object bytes are not valid ELF"
        ));
        return;
    }
    if bytes[4] != 2 {
        errors.push(format!(
            "bad_object_artifact: {cache_key} object bytes are not ELF64"
        ));
    }
    if bytes[5] != 1 {
        errors.push(format!(
            "bad_object_artifact: {cache_key} object bytes are not little-endian ELF"
        ));
    }
    let object_type = u16::from_le_bytes([bytes[16], bytes[17]]);
    if object_type != 1 {
        errors.push(format!(
            "bad_object_artifact: {cache_key} ELF object is not relocatable"
        ));
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if machine != 62 {
        errors.push(format!(
            "bad_object_artifact: {cache_key} ELF object is not x86_64"
        ));
    }
}

fn verify_macho_object_header(errors: &mut Vec<String>, cache_key: &str, bytes: &[u8]) {
    if bytes.len() < 32 || !bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        errors.push(format!(
            "bad_object_artifact: {cache_key} object bytes are not valid Mach-O"
        ));
        return;
    }
    let cpu_type = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if cpu_type != 0x0100000c {
        errors.push(format!(
            "bad_object_artifact: {cache_key} Mach-O object is not arm64"
        ));
    }
    let file_type = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    if file_type != 1 {
        errors.push(format!(
            "bad_object_artifact: {cache_key} Mach-O object is not relocatable"
        ));
    }
}

fn required_object_byte_symbols(target_triple: &str, metadata: &JsonValue) -> BTreeSet<String> {
    let mut symbols = BTreeSet::new();
    match target_triple {
        crate::LINUX_X86_64_TARGET => {
            symbols.extend(json_string_set(metadata.get("defined_symbols")));
            symbols.extend(
                metadata
                    .get("relocations")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|relocation| {
                        relocation
                            .get("target_abi_symbol")
                            .and_then(JsonValue::as_str)
                    })
                    .map(str::to_string),
            );
        }
        crate::APPLE_ARM64_TARGET => {
            let object_symbols = json_string_set(metadata.get("object_symbols"));
            if object_symbols.is_empty() {
                symbols.extend(
                    json_string_set(metadata.get("defined_symbols"))
                        .into_iter()
                        .map(|symbol| format!("_{symbol}")),
                );
            } else {
                symbols.extend(object_symbols);
            }
            for relocation in metadata
                .get("relocations")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(object_symbol) = relocation
                    .get("target_object_symbol")
                    .and_then(JsonValue::as_str)
                {
                    symbols.insert(object_symbol.to_string());
                } else if let Some(abi_symbol) = relocation
                    .get("target_abi_symbol")
                    .and_then(JsonValue::as_str)
                {
                    symbols.insert(format!("_{abi_symbol}"));
                }
            }
        }
        _ => {}
    }
    symbols
}

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn verify_link_plan_object_matches_object_metadata(
    errors: &mut Vec<String>,
    plan_cache_key: &str,
    object: &JsonValue,
    metadata: &JsonValue,
) {
    for (label, plan_key, metadata_key) in [
        ("symbol", "symbol_hash", "symbol_hash"),
        ("definition", "definition_hash", "function_def_hash"),
        ("signature", "signature_hash", "function_sig_hash"),
        ("object format", "object_format", "object_format"),
        ("defined symbols", "defined_symbols", "defined_symbols"),
        ("called symbols", "called_symbols", "called_symbols"),
        ("relocations", "relocations", "relocations"),
        ("debug metadata", "debug_metadata", "debug_metadata"),
    ] {
        if object.get(plan_key) != metadata.get(metadata_key) {
            errors.push(format!(
                "bad_link_plan: {plan_cache_key} object {label} does not match object artifact metadata"
            ));
        }
    }

    let plan_object_symbols = object
        .get("object_symbols")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let metadata_object_symbols = metadata
        .get("object_symbols")
        .cloned()
        .unwrap_or_else(|| json!([]));
    if plan_object_symbols != metadata_object_symbols {
        errors.push(format!(
            "bad_link_plan: {plan_cache_key} object symbols do not match object artifact metadata"
        ));
    }
}

fn verify_json_field_matches(
    errors: &mut Vec<String>,
    label: &str,
    cache_key: &str,
    left: &JsonValue,
    left_field: &str,
    right: &JsonValue,
    right_field: &str,
) {
    if left.get(left_field).and_then(JsonValue::as_str)
        != right.get(right_field).and_then(JsonValue::as_str)
    {
        errors.push(format!(
            "{label}: {cache_key} {left_field} metadata mismatch"
        ));
    }
}

fn artifact_text(artifact_json: &JsonValue) -> Option<&str> {
    if artifact_json.get("schema").and_then(JsonValue::as_str) == Some(ARTIFACT_METADATA_SCHEMA) {
        return artifact_json.get("text").and_then(JsonValue::as_str);
    }
    artifact_json.get("text").and_then(JsonValue::as_str)
}

fn artifact_inner_metadata(artifact_json: &JsonValue) -> Option<&JsonValue> {
    if artifact_json.get("schema").and_then(JsonValue::as_str) == Some(ARTIFACT_METADATA_SCHEMA) {
        artifact_json.get("metadata")
    } else {
        Some(artifact_json)
    }
}

fn json_string_set(value: Option<&JsonValue>) -> BTreeSet<String> {
    value
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValue::as_str)
        .map(str::to_string)
        .collect()
}

fn json_string_vec(value: Option<&JsonValue>) -> Option<Vec<String>> {
    value.map(|value| {
        value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
    })?
}

fn json_value_set(value: Option<&JsonValue>) -> BTreeSet<String> {
    value
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .map(canonical_json)
        .collect()
}

fn link_plan_object_symbols(plan: &JsonValue) -> Vec<String> {
    plan.get("objects")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(|object| object.get("symbol_hash").and_then(JsonValue::as_str))
        .map(str::to_string)
        .collect()
}

fn link_plan_external_symbols(plan: &JsonValue) -> BTreeSet<String> {
    plan.get("external_symbols")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(|object| object.get("symbol_hash").and_then(JsonValue::as_str))
        .map(str::to_string)
        .collect()
}

fn json_array_contains_str(value: Option<&JsonValue>, needle: &str) -> bool {
    value
        .and_then(JsonValue::as_array)
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(needle)))
}
