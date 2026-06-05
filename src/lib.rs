mod abi;
mod api;
mod artifact;
mod backend;
mod backend_c;
mod branches;
mod build_plan;
mod bundle;
pub mod debugger;
mod diff;
mod expr;
mod jobs;
mod layout;
mod link;
mod lowering;
mod merge;
mod migrations;
mod model;
mod patch;
mod provenance;
pub mod server;
mod store;
mod tests;
pub mod trace;
mod types;
mod verify;
pub mod workspace;

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use serde_json::json;

pub use expr::{
    ExternalFunctionSource, FunctionSource, ProgramItem, RawExpr, TypeDefinitionSource, Value,
};
pub use store::CodeDb;
pub use types::{Effect, ParamSpec, TypeDefinitionKind, TypeMemberSpec};

use backend::ArtifactKind;
use expr::{parse_expr_source, parse_program, parse_signature_source_with_effects};
use migrations::Operation;
use model::{param_names, preferred_names, preferred_type_names, root_module_names};

pub(crate) const SCHEMA_SQL: &str = include_str!("../schema.sql");
pub(crate) const OBJECT_DOMAIN: &[u8] = b"codedb/object/v1\0";
pub(crate) const MIGRATION_DOMAIN: &[u8] = b"codedb/migration/v1\0";
pub(crate) const HISTORY_DOMAIN: &[u8] = b"codedb/history/v1\0";
pub(crate) const CACHE_DOMAIN: &[u8] = b"codedb/cache/v1\0";
pub(crate) const BYTES_DOMAIN: &[u8] = b"codedb/bytes/v1\0";
pub(crate) const SCHEMA_VERSION: i64 = 1;
pub(crate) const MAIN_BRANCH: &str = "main";
pub const LINUX_X86_64_TARGET: &str = "x86_64-unknown-linux-gnu";
pub const APPLE_ARM64_TARGET: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const DEFAULT_NATIVE_TARGET: &str = APPLE_ARM64_TARGET;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub const DEFAULT_NATIVE_TARGET: &str = LINUX_X86_64_TARGET;
pub(crate) const ABI_TAG: &str = "codedb-v0-internal";
pub(crate) const COMPILER_VERSION: &str = concat!("codedb-", env!("CARGO_PKG_VERSION"));
pub(crate) const PIPELINE_VERSION: &str = "pipeline:v1";

pub(crate) fn parse_eval_arg(arg: &str, type_name: &str, idx: usize) -> Result<Value> {
    match type_name {
        "i64" => arg
            .parse::<i64>()
            .map(Value::I64)
            .with_context(|| format!("argument {idx} must be i64, got {arg:?}")),
        "bool" => match arg {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => bail!("argument {idx} must be bool literal true or false, got {arg:?}"),
        },
        "unit" => match arg {
            "()" | "unit" => Ok(Value::Unit),
            _ => bail!("argument {idx} must be unit literal () or unit, got {arg:?}"),
        },
        other => bail!("unsupported parameter type {other}"),
    }
}

impl CodeDb {
    pub fn init(&mut self) -> Result<String> {
        self.ensure_initialized()
    }

    pub fn import_file(&mut self, path: &Path) -> Result<String> {
        self.ensure_initialized()?;
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let items = parse_program(&source)?;
        let mut report = String::new();

        for (idx, item) in items.into_iter().enumerate() {
            let branch = self.branch(MAIN_BRANCH)?;
            let op = match item {
                ProgramItem::TypeDefinition(definition) => {
                    let birth_seed = format!(
                        "import:type:{}:{}:{}",
                        definition.module, definition.name, idx
                    );
                    Operation::CreateType {
                        module: definition.module,
                        name: definition.name,
                        birth_seed,
                        region_params: definition.region_params,
                        definition: definition.definition,
                        identity: definition.identity,
                    }
                }
                ProgramItem::Function(function) => {
                    let birth_seed =
                        format!("import:{}:{}:{}", function.module, function.name, idx);
                    Operation::CreateFunction {
                        module: function.module,
                        name: function.name,
                        birth_seed,
                        region_params: function.region_params,
                        params: function.params,
                        return_type: function.return_type,
                        effects: function.effects,
                        body: function.body,
                    }
                }
                ProgramItem::ExternalFunction(function) => {
                    let birth_seed = format!(
                        "import:extern:{}:{}:{}",
                        function.module, function.name, idx
                    );
                    Operation::CreateExternalFunction {
                        module: function.module,
                        name: function.name,
                        birth_seed,
                        region_params: function.region_params,
                        params: function.params,
                        return_type: function.return_type,
                        effects: function.effects,
                        abi: function.abi,
                        link_name: function.link_name,
                        library: function.library,
                    }
                }
            };
            let outcome = self.apply_and_record(branch, op)?;
            report.push_str(&outcome.format_cli());
        }

        let branch = self.branch(MAIN_BRANCH)?;
        report.push_str(&format!("root {}\n", branch.root_hash));
        if let Some(history) = branch.history_hash {
            report.push_str(&format!("history {history}\n"));
        }
        Ok(report)
    }

    pub fn export_branch(&mut self, branch: &str) -> Result<String> {
        self.ensure_initialized()?;
        let root_hash = self.branch(branch)?.root_hash;
        let source = self.render_source(&root_hash)?;
        self.write_cache_text(
            &root_hash,
            "projection",
            "canonical_source",
            ArtifactKind::CanonicalSource,
            &source,
        )?;
        Ok(source)
    }

    pub fn eval_main_branch(&self, function_name: &str, args: Vec<Value>) -> Result<Value> {
        let branch = self.branch(MAIN_BRANCH)?;
        self.eval_name(&branch.root_hash, function_name, args)
    }

    pub fn eval_main_branch_text_args(
        &self,
        function_name: &str,
        args: &[String],
    ) -> Result<Value> {
        let branch = self.branch(MAIN_BRANCH)?;
        let root = self.load_root(&branch.root_hash)?;
        let symbol = self.resolve_symbol_or_name(&branch.root_hash, function_name)?;
        let root_symbol = self
            .root_symbol(&root, &symbol)
            .ok_or_else(|| anyhow!("missing symbol {symbol}"))?;
        let (param_types, _) = self.signature_parts(&root_symbol.signature)?;
        if param_types.len() != args.len() {
            bail!(
                "{function_name} expects {} args, got {}",
                param_types.len(),
                args.len()
            );
        }

        let parsed_args = args
            .iter()
            .zip(param_types.iter())
            .enumerate()
            .map(|(idx, (arg, type_hash))| {
                let type_name = self.type_name(type_hash)?;
                parse_eval_arg(arg, &type_name, idx)
            })
            .collect::<Result<Vec<_>>>()?;
        self.eval_symbol(&branch.root_hash, &symbol, parsed_args)
    }

    pub fn emit_c_main_branch(&mut self, function_name: &str) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        self.resolve_symbol_or_name(&branch.root_hash, function_name)
            .with_context(|| format!("unknown entry function {function_name}"))?;
        let source = self.render_c(&branch.root_hash)?;
        self.write_cache_text(
            &branch.root_hash,
            "projection",
            "c_source",
            ArtifactKind::CProjection,
            &source,
        )?;
        Ok(source)
    }

    pub fn list_main_branch(&self) -> Result<String> {
        let branch = self.branch(MAIN_BRANCH)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut out = String::new();
        for binding in preferred_type_names(&root) {
            let root_type = root
                .types
                .iter()
                .find(|entry| entry.type_symbol == binding.type_symbol)
                .ok_or_else(|| {
                    anyhow!(
                        "root type name points to missing type {}",
                        binding.type_symbol
                    )
                })?;
            let definition = self.type_definition(&root_type.type_def)?;
            out.push_str(&format!(
                "{}.{} {} type {}\n",
                binding.module,
                binding.display_name,
                binding.type_symbol,
                definition.kind_name()
            ));
        }
        for binding in preferred_names(&root) {
            let symbol = binding.symbol;
            let root_symbol = root
                .symbols
                .iter()
                .find(|entry| entry.symbol == symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {symbol}"))?;
            let signature = self.signature_source_in_root(
                &root,
                &binding.module,
                &root_symbol.signature,
                &param_names(&root, &symbol),
            )?;
            let prefix = if self.definition_is_external(&root_symbol.definition)? {
                "extern fn"
            } else {
                "fn"
            };
            out.push_str(&format!(
                "{}.{} {} {} {}\n",
                binding.module, binding.display_name, symbol, prefix, signature
            ));
        }
        Ok(out)
    }

    pub fn list_main_branch_json(&self) -> Result<String> {
        self.list_branch_json(MAIN_BRANCH)
    }

    pub(crate) fn list_branch_json(&self, branch_name: &str) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut types = Vec::new();
        for binding in preferred_type_names(&root) {
            let root_type = root
                .types
                .iter()
                .find(|entry| entry.type_symbol == binding.type_symbol)
                .ok_or_else(|| {
                    anyhow!(
                        "root type name points to missing type {}",
                        binding.type_symbol
                    )
                })?;
            let definition = self.type_definition(&root_type.type_def)?;
            let kind = definition.kind_name();
            let (region_params, members_key, members) = match definition {
                types::TypeDefinition::Record {
                    region_params,
                    fields,
                    ..
                } => (
                    region_params,
                    "fields",
                    fields
                        .into_iter()
                        .map(|field| {
                            json!({
                                "name": field.name,
                                "symbol_hash": field.member_symbol,
                                "type_hash": field.type_hash,
                            })
                        })
                        .collect::<Vec<_>>(),
                ),
                types::TypeDefinition::Enum {
                    region_params,
                    variants,
                    ..
                } => (
                    region_params,
                    "variants",
                    variants
                        .into_iter()
                        .map(|variant| {
                            json!({
                                "name": variant.name,
                                "symbol_hash": variant.member_symbol,
                                "type_hash": variant.type_hash,
                            })
                        })
                        .collect::<Vec<_>>(),
                ),
            };
            let mut object = serde_json::Map::new();
            object.insert("module".to_string(), json!(binding.module));
            object.insert("name".to_string(), json!(binding.display_name));
            object.insert("type_symbol_hash".to_string(), json!(binding.type_symbol));
            object.insert("type_def_hash".to_string(), json!(root_type.type_def));
            object.insert("kind".to_string(), json!(kind));
            object.insert(
                "region_params".to_string(),
                json!(
                    region_params
                        .into_iter()
                        .map(|param| json!({
                            "name": param.name,
                            "region_hash": param.region,
                        }))
                        .collect::<Vec<_>>()
                ),
            );
            object.insert(members_key.to_string(), json!(members));
            types.push(serde_json::Value::Object(object));
        }
        let mut symbols = Vec::new();
        for binding in preferred_names(&root) {
            let symbol = binding.symbol;
            let root_symbol = root
                .symbols
                .iter()
                .find(|entry| entry.symbol == symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {symbol}"))?;
            let external = if self.definition_is_external(&root_symbol.definition)? {
                let metadata = self.external_function_metadata(&root_symbol.definition)?;
                json!({
                    "abi": metadata.abi,
                    "link_name": metadata.link_name,
                    "library": metadata.library,
                })
            } else {
                json!(null)
            };
            symbols.push(json!({
                "module": binding.module,
                "name": binding.display_name,
                "symbol_hash": symbol,
                "signature_hash": root_symbol.signature,
                "definition_hash": root_symbol.definition,
                "definition_kind": if external.is_null() { "function" } else { "external_function" },
                "external": external,
                "effects": self.signature_effect_names(&root_symbol.signature)?,
                "signature": self.signature_source_in_root(&root, &binding.module, &root_symbol.signature, &param_names(&root, &symbol))?,
            }));
        }
        Ok(format!(
            "{}\n",
            store::canonical_json(&json!({
                "branch": branch_name,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
                "types": types,
                "symbols": symbols,
            }))
        ))
    }

    pub fn show_main_branch(&self, symbol_or_name: &str) -> Result<String> {
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self.resolve_symbol_or_name(&branch.root_hash, symbol_or_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let binding = self
            .preferred_binding(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol has no preferred name {symbol}"))?;
        let root_symbol = self
            .root_symbol(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol missing from root {symbol}"))?;
        let deps = self.dependencies_for_symbol(&branch.root_hash, &symbol)?;
        let mut out = String::new();
        out.push_str(&format!("symbol {symbol}\n"));
        out.push_str(&format!(
            "name {}.{}\n",
            binding.module, binding.display_name
        ));
        out.push_str(&format!("signature {}\n", root_symbol.signature));
        out.push_str(&format!(
            "effects {}\n",
            self.signature_effect_names(&root_symbol.signature)?
                .join(",")
        ));
        out.push_str(&format!("definition {}\n", root_symbol.definition));
        if self.definition_is_external(&root_symbol.definition)? {
            let external = self.external_function_metadata(&root_symbol.definition)?;
            out.push_str("definition_kind external_function\n");
            out.push_str(&format!("external_abi {}\n", external.abi));
            out.push_str(&format!("external_link_name {}\n", external.link_name));
            out.push_str(&format!(
                "external_library {}\n",
                external.library.unwrap_or_else(|| "none".to_string())
            ));
            out.push_str(&format!(
                "source {}\n",
                self.render_function_source(&root, binding, root_symbol)?
            ));
        } else {
            let body_hash = self.function_body_hash(&root_symbol.definition)?;
            out.push_str("definition_kind function\n");
            out.push_str(&format!("body {body_hash}\n"));
            out.push_str(&format!(
                "source fn {}{}\n",
                binding.display_name,
                self.signature_source_in_root(
                    &root,
                    &binding.module,
                    &root_symbol.signature,
                    &param_names(&root, &symbol),
                )?
            ));
            out.push_str(&format!(
                "body_source {}\n",
                self.expr_to_source_in_module(
                    &body_hash,
                    &root,
                    &binding.module,
                    &param_names(&root, &symbol),
                    0
                )?
            ));
        }
        out.push_str(&format!(
            "internal_abi_symbol {}\n",
            abi::internal_abi_symbol(&symbol)?
        ));
        let exports = abi::exported_abi_names(&root, &symbol);
        if exports.is_empty() {
            out.push_str("exported_abi_symbols none\n");
        } else {
            out.push_str(&format!("exported_abi_symbols {}\n", exports.join(",")));
        }
        if deps.is_empty() {
            out.push_str("dependencies none\n");
        } else {
            for dep in deps {
                let name = self.symbol_display(&root, &dep)?;
                out.push_str(&format!("depends_on {name} {dep}\n"));
            }
        }
        Ok(out)
    }

    pub fn show_main_branch_json(&self, symbol_or_name: &str) -> Result<String> {
        self.show_branch_json(MAIN_BRANCH, symbol_or_name)
    }

    pub(crate) fn show_branch_json(
        &self,
        branch_name: &str,
        symbol_or_name: &str,
    ) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let symbol = self.resolve_symbol_or_name(&branch.root_hash, symbol_or_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let binding = self
            .preferred_binding(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol has no preferred name {symbol}"))?;
        let root_symbol = self
            .root_symbol(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol missing from root {symbol}"))?;
        let deps = self.dependencies_for_symbol(&branch.root_hash, &symbol)?;
        let dependencies = deps
            .iter()
            .map(|dep| {
                let binding = self
                    .preferred_binding(&root, dep)
                    .ok_or_else(|| anyhow!("symbol has no preferred name {dep}"))?;
                Ok(json!({
                    "module": binding.module,
                    "name": binding.display_name,
                    "qualified_name": format!("{}.{}", binding.module, binding.display_name),
                    "symbol_hash": dep,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let local_param_names = param_names(&root, &symbol);
        let external = if self.definition_is_external(&root_symbol.definition)? {
            let metadata = self.external_function_metadata(&root_symbol.definition)?;
            json!({
                "abi": metadata.abi,
                "link_name": metadata.link_name,
                "library": metadata.library,
            })
        } else {
            json!(null)
        };
        let (body_hash, body_source) = if external.is_null() {
            let body_hash = self.function_body_hash(&root_symbol.definition)?;
            let body_source = self.expr_to_source_in_module(
                &body_hash,
                &root,
                &binding.module,
                &local_param_names,
                0,
            )?;
            (json!(body_hash), json!(body_source))
        } else {
            (json!(null), json!(null))
        };
        let payload = json!({
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "symbol_hash": symbol,
            "module": binding.module,
            "name": binding.display_name,
            "signature_hash": root_symbol.signature,
            "definition_hash": root_symbol.definition,
            "definition_kind": if external.is_null() { "function" } else { "external_function" },
            "body_hash": body_hash,
            "external": external,
            "effects": self.signature_effect_names(&root_symbol.signature)?,
            "internal_abi_symbol": abi::internal_abi_symbol(&symbol)?,
            "exported_abi_symbols": abi::exported_abi_names(&root, &symbol),
            "signature": self.signature_source_in_root(&root, &binding.module, &root_symbol.signature, &local_param_names)?,
            "source": self.render_function_source(&root, binding, root_symbol)?,
            "body_source": body_source,
            "dependencies": dependencies,
        });
        Ok(format!("{}\n", store::canonical_json(&payload)))
    }

    pub fn callers_main_branch(&self, symbol_or_name: &str) -> Result<String> {
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self.resolve_symbol_or_name(&branch.root_hash, symbol_or_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut stmt = self.conn.prepare(
            "SELECT from_symbol_hash FROM dependencies WHERE root_hash = ?1 AND to_symbol_hash = ?2 ORDER BY from_symbol_hash",
        )?;
        let callers = stmt
            .query_map(params![branch.root_hash, symbol], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = String::new();
        for caller in callers {
            out.push_str(&format!("{}\n", self.symbol_display(&root, &caller)?));
        }
        Ok(out)
    }

    pub fn rename_main_branch(&mut self, old_name: &str, new_name: &str) -> Result<String> {
        self.rename_main_branch_expected(old_name, new_name, None)
    }

    pub fn rename_main_branch_expected(
        &mut self,
        old_name: &str,
        new_name: &str,
        expected_root: Option<&str>,
    ) -> Result<String> {
        self.rename_main_branch_expected_format(old_name, new_name, expected_root, false)
    }

    pub fn rename_main_branch_expected_format(
        &mut self,
        old_name: &str,
        new_name: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = match self.resolve_name(&operation_root, "main", old_name) {
            Ok(symbol) => symbol,
            Err(err) if expected_root.is_none() => self
                .resolve_name(&branch.root_hash, "main", new_name)
                .map_err(|_| err)?,
            Err(err) => return Err(err),
        };
        let op = Operation::RenameSymbol {
            module: "main".to_string(),
            symbol,
            old_name: old_name.to_string(),
            new_name: new_name.to_string(),
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn replace_body_main_branch(&mut self, name: &str, expr: &str) -> Result<String> {
        self.replace_body_main_branch_expected(name, expr, None)
    }

    pub fn replace_body_main_branch_expected(
        &mut self,
        name: &str,
        expr: &str,
        expected_root: Option<&str>,
    ) -> Result<String> {
        self.replace_body_main_branch_expected_format(name, expr, expected_root, false)
    }

    pub fn replace_body_main_branch_expected_format(
        &mut self,
        name: &str,
        expr: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = self.resolve_name(&operation_root, "main", name)?;
        let body = parse_expr_source(expr)?;
        let op = Operation::ReplaceFunctionBody {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            body,
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn change_signature_main_branch(&mut self, name: &str, signature: &str) -> Result<String> {
        self.change_signature_main_branch_expected(name, signature, None)
    }

    pub fn change_signature_main_branch_expected(
        &mut self,
        name: &str,
        signature: &str,
        expected_root: Option<&str>,
    ) -> Result<String> {
        self.change_signature_main_branch_expected_format(name, signature, expected_root, false)
    }

    pub fn change_signature_main_branch_expected_format(
        &mut self,
        name: &str,
        signature: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = self.resolve_name(&operation_root, "main", name)?;
        let (params, return_type, effects) = parse_signature_source_with_effects(signature)?;
        let op = Operation::ChangeFunctionSignature {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            region_params: Vec::new(),
            params,
            return_type,
            effects,
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn delete_symbol_main_branch(&mut self, name: &str, force: bool) -> Result<String> {
        self.delete_symbol_main_branch_expected(name, force, None)
    }

    pub fn delete_symbol_main_branch_expected(
        &mut self,
        name: &str,
        force: bool,
        expected_root: Option<&str>,
    ) -> Result<String> {
        self.delete_symbol_main_branch_expected_format(name, force, expected_root, false)
    }

    pub fn delete_symbol_main_branch_expected_format(
        &mut self,
        name: &str,
        force: bool,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = self.resolve_name(&operation_root, "main", name)?;
        let op = Operation::DeleteSymbol {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            force,
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn create_alias_main_branch(&mut self, name: &str, alias: &str) -> Result<String> {
        self.create_alias_main_branch_expected(name, alias, None)
    }

    pub fn create_alias_main_branch_expected(
        &mut self,
        name: &str,
        alias: &str,
        expected_root: Option<&str>,
    ) -> Result<String> {
        self.create_alias_main_branch_expected_format(name, alias, expected_root, false)
    }

    pub fn create_alias_main_branch_expected_format(
        &mut self,
        name: &str,
        alias: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = self.resolve_name(&operation_root, "main", name)?;
        let op = Operation::CreateAlias {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            alias: alias.to_string(),
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn remove_alias_main_branch(&mut self, name: &str, alias: &str) -> Result<String> {
        self.remove_alias_main_branch_expected(name, alias, None)
    }

    pub fn remove_alias_main_branch_expected(
        &mut self,
        name: &str,
        alias: &str,
        expected_root: Option<&str>,
    ) -> Result<String> {
        self.remove_alias_main_branch_expected_format(name, alias, expected_root, false)
    }

    pub fn remove_alias_main_branch_expected_format(
        &mut self,
        name: &str,
        alias: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = self.resolve_name(&operation_root, "main", name)?;
        let op = Operation::RemoveAlias {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            alias: alias.to_string(),
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn set_export_main_branch(&mut self, name: &str, exported_name: &str) -> Result<String> {
        self.set_export_main_branch_expected(name, exported_name, None)
    }

    pub fn set_export_main_branch_expected(
        &mut self,
        name: &str,
        exported_name: &str,
        expected_root: Option<&str>,
    ) -> Result<String> {
        self.set_export_main_branch_expected_format(name, exported_name, expected_root, false)
    }

    pub fn set_export_main_branch_expected_format(
        &mut self,
        name: &str,
        exported_name: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = self.resolve_name(&operation_root, "main", name)?;
        let op = Operation::SetExport {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            exported_name: exported_name.to_string(),
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn remove_export_main_branch(&mut self, name: &str, exported_name: &str) -> Result<String> {
        self.remove_export_main_branch_expected(name, exported_name, None)
    }

    pub fn remove_export_main_branch_expected(
        &mut self,
        name: &str,
        exported_name: &str,
        expected_root: Option<&str>,
    ) -> Result<String> {
        self.remove_export_main_branch_expected_format(name, exported_name, expected_root, false)
    }

    pub fn remove_export_main_branch_expected_format(
        &mut self,
        name: &str,
        exported_name: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = self.resolve_name(&operation_root, "main", name)?;
        let op = Operation::RemoveExport {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            exported_name: exported_name.to_string(),
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn list_modules_main_branch(&self) -> Result<String> {
        self.list_modules_branch(MAIN_BRANCH)
    }

    pub fn list_modules_main_branch_json(&self) -> Result<String> {
        self.list_modules_branch_json(MAIN_BRANCH)
    }

    pub(crate) fn list_modules_branch(&self, branch_name: &str) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut out = String::new();
        for module in root_module_names(&root) {
            let symbol_count = root
                .names
                .iter()
                .filter(|binding| binding.module == module && binding.is_preferred)
                .count();
            out.push_str(&format!("{module} symbols {symbol_count}\n"));
        }
        Ok(out)
    }

    pub(crate) fn list_modules_branch_json(&self, branch_name: &str) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let modules = root_module_names(&root)
            .into_iter()
            .map(|module| {
                let symbols = preferred_names(&root)
                    .into_iter()
                    .filter(|binding| binding.module == module)
                    .map(|binding| {
                        json!({
                            "name": binding.display_name,
                            "symbol_hash": binding.symbol,
                        })
                    })
                    .collect::<Vec<_>>();
                json!({
                    "name": module,
                    "symbol_count": symbols.len(),
                    "symbols": symbols,
                })
            })
            .collect::<Vec<_>>();
        Ok(format!(
            "{}\n",
            store::canonical_json(&json!({
                "schema": "codedb/modules/v1",
                "branch": branch_name,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
                "modules": modules,
            }))
        ))
    }

    pub fn show_module_main_branch(&self, module: &str) -> Result<String> {
        self.show_module_branch(MAIN_BRANCH, module)
    }

    pub fn show_module_main_branch_json(&self, module: &str) -> Result<String> {
        self.show_module_branch_json(MAIN_BRANCH, module)
    }

    pub(crate) fn show_module_branch(&self, branch_name: &str, module: &str) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        if !root_module_names(&root).contains(module) {
            anyhow::bail!("unknown module {module}");
        }
        let mut out = String::new();
        out.push_str(&format!("module {module}\n"));
        out.push_str(&format!("root {}\n", branch.root_hash));
        for binding in preferred_names(&root)
            .into_iter()
            .filter(|binding| binding.module == module)
        {
            out.push_str(&format!(
                "symbol {} {}\n",
                binding.display_name, binding.symbol
            ));
        }
        Ok(out)
    }

    pub(crate) fn show_module_branch_json(
        &self,
        branch_name: &str,
        module: &str,
    ) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        if !root_module_names(&root).contains(module) {
            anyhow::bail!("unknown module {module}");
        }
        let symbols = preferred_names(&root)
            .into_iter()
            .filter(|binding| binding.module == module)
            .map(|binding| {
                json!({
                    "name": binding.display_name,
                    "symbol_hash": binding.symbol,
                })
            })
            .collect::<Vec<_>>();
        Ok(format!(
            "{}\n",
            store::canonical_json(&json!({
                "schema": "codedb/module/v1",
                "branch": branch_name,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
                "module": module,
                "symbol_count": symbols.len(),
                "symbols": symbols,
            }))
        ))
    }

    pub fn move_symbol_main_branch_expected_format(
        &mut self,
        symbol_or_name: &str,
        new_module: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let symbol = self.resolve_symbol_or_name(&operation_root, symbol_or_name)?;
        let root = self.load_root(&operation_root)?;
        let binding = self
            .preferred_binding(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol has no preferred name {symbol}"))?;
        let op = Operation::MoveSymbol {
            module: binding.module.clone(),
            symbol,
            name: binding.display_name.clone(),
            new_module: new_module.to_string(),
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(format_outcome(outcome, json))
    }

    pub fn export_map_main_branch(&self) -> Result<String> {
        let branch = self.branch(MAIN_BRANCH)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut out = String::new();
        for binding in preferred_names(&root) {
            let exports = abi::exported_abi_names(&root, &binding.symbol);
            out.push_str(&format!(
                "{}.{} {} internal_abi_symbol {} exported_abi_symbols {}\n",
                binding.module,
                binding.display_name,
                binding.symbol,
                abi::internal_abi_symbol(&binding.symbol)?,
                if exports.is_empty() {
                    "none".to_string()
                } else {
                    exports.join(",")
                }
            ));
        }
        Ok(out)
    }

    pub fn export_map_main_branch_json(&self) -> Result<String> {
        let branch = self.branch(MAIN_BRANCH)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut entries = Vec::new();
        for binding in preferred_names(&root) {
            entries.push(json!({
                "module": binding.module,
                "name": binding.display_name,
                "symbol_hash": binding.symbol,
                "internal_abi_symbol": abi::internal_abi_symbol(&binding.symbol)?,
                "exported_abi_symbols": abi::exported_abi_names(&root, &binding.symbol),
            }));
        }
        Ok(format!(
            "{}\n",
            store::canonical_json(&json!({
                "branch": MAIN_BRANCH,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
                "exports": entries,
            }))
        ))
    }
}

fn format_outcome(outcome: migrations::MigrationOutcome, json: bool) -> String {
    if json {
        outcome.format_json()
    } else {
        outcome.format_cli()
    }
}
