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
mod op_registry;
pub mod oracle;
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
pub use op_registry::operator_kinds;
pub use store::CodeDb;
pub use types::{Effect, ParamSpec, TypeDefinitionKind, TypeMemberSpec};

use backend::ArtifactKind;
use expr::{parse_expr_source, parse_program, parse_signature_source_with_effects};
use migrations::{Operation, RecursionGroupMemberSpec};
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

    /// Recursion analysis of a parsed program: which function items belong to a
    /// mutually-recursive clique that must be created atomically (SPEC_V3 §6).
    /// Non-recursive functions are absent, so their import is byte-for-byte
    /// unchanged (no migration-history churn for existing programs).
    fn analyze_recursion_groups(items: &[ProgramItem]) -> RecursionAnalysis {
        // Node space = function items; map their (module, name) for call resolution.
        let mut func_items: Vec<usize> = Vec::new();
        let mut name_to_node: std::collections::HashMap<(String, String), usize> =
            std::collections::HashMap::new();
        for (idx, item) in items.iter().enumerate() {
            if let ProgramItem::Function(function) = item {
                let node = func_items.len();
                func_items.push(idx);
                name_to_node.insert((function.module.clone(), function.name.clone()), node);
            }
        }
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); func_items.len()];
        for (node, &item_idx) in func_items.iter().enumerate() {
            let ProgramItem::Function(function) = &items[item_idx] else {
                continue;
            };
            let mut names = Vec::new();
            collect_call_names(&function.body, &mut names);
            for name in names {
                if let Some(target) = resolve_call_node(&name, &function.module, &name_to_node)
                    && !adjacency[node].contains(&target)
                {
                    adjacency[node].push(target);
                }
            }
        }
        let mut analysis = RecursionAnalysis::default();
        for component in tarjan_scc(&adjacency) {
            let recursive = component.len() > 1
                || (component.len() == 1 && adjacency[component[0]].contains(&component[0]));
            if !recursive {
                continue;
            }
            let members: Vec<usize> = component.iter().map(|&node| func_items[node]).collect();
            // Order members canonically (by clique structure, not source position)
            // so the group's content identity is order-independent and
            // import→export→import is a fixpoint (SPEC_V3 §6/§10).
            let members = canonical_recursion_member_order(items, &members);
            let group_id = analysis.groups.len();
            for &member in &members {
                analysis.group_of.insert(member, group_id);
            }
            analysis.groups.push(members);
        }
        analysis
    }

    pub fn import_file(&mut self, path: &Path) -> Result<String> {
        self.ensure_initialized()?;
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let items = parse_program(&source)?;
        let mut report = String::new();

        // Detect mutually-recursive function cliques up front so they can be
        // created atomically (SPEC_V3 §6). Non-recursive items keep their
        // original one-op-per-item lowering — unchanged migration history.
        let recursion = Self::analyze_recursion_groups(&items);
        let mut emitted_group = vec![false; recursion.groups.len()];

        for (idx, item) in items.iter().enumerate() {
            let branch = self.branch(MAIN_BRANCH)?;
            let op = match item {
                ProgramItem::TypeDefinition(definition) => {
                    let birth_seed = format!(
                        "import:type:{}:{}:{}",
                        definition.module, definition.name, idx
                    );
                    Operation::CreateType {
                        module: definition.module.clone(),
                        name: definition.name.clone(),
                        birth_seed,
                        region_params: definition.region_params.clone(),
                        definition: definition.definition.clone(),
                        identity: definition.identity.clone(),
                    }
                }
                ProgramItem::Function(function) => {
                    if let Some(&group_id) = recursion.group_of.get(&idx) {
                        // A recursive clique is created once, at its first member,
                        // with every member bound before any body is typed.
                        if emitted_group[group_id] {
                            continue;
                        }
                        emitted_group[group_id] = true;
                        let members = &recursion.groups[group_id];
                        let module = function.module.clone();
                        let mut member_specs = Vec::with_capacity(members.len());
                        for &member_idx in members {
                            let ProgramItem::Function(member) = &items[member_idx] else {
                                unreachable!("recursion-group member is not a function");
                            };
                            if member.module != module {
                                bail!(
                                    "cross-module recursion is not supported: clique spans modules {module} and {}",
                                    member.module
                                );
                            }
                            member_specs.push(RecursionGroupMemberSpec {
                                name: member.name.clone(),
                                region_params: member.region_params.clone(),
                                params: member.params.clone(),
                                return_type: member.return_type.clone(),
                                effects: member.effects.clone(),
                                body: member.body.clone(),
                            });
                        }
                        Operation::CreateRecursionGroup {
                            module,
                            members: member_specs,
                        }
                    } else {
                        let birth_seed =
                            format!("import:{}:{}:{}", function.module, function.name, idx);
                        Operation::CreateFunction {
                            module: function.module.clone(),
                            name: function.name.clone(),
                            birth_seed,
                            region_params: function.region_params.clone(),
                            params: function.params.clone(),
                            return_type: function.return_type.clone(),
                            effects: function.effects.clone(),
                            body: function.body.clone(),
                        }
                    }
                }
                ProgramItem::ExternalFunction(function) => {
                    let birth_seed = format!(
                        "import:extern:{}:{}:{}",
                        function.module, function.name, idx
                    );
                    Operation::CreateExternalFunction {
                        module: function.module.clone(),
                        name: function.name.clone(),
                        birth_seed,
                        region_params: function.region_params.clone(),
                        params: function.params.clone(),
                        return_type: function.return_type.clone(),
                        effects: function.effects.clone(),
                        abi: function.abi.clone(),
                        link_name: function.link_name.clone(),
                        library: function.library.clone(),
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

/// Which parsed function items form mutually-recursive cliques (SPEC_V3 §6).
#[derive(Default)]
struct RecursionAnalysis {
    /// item index -> group id (only for items in a recursive clique).
    group_of: std::collections::HashMap<usize, usize>,
    /// group id -> member item indices, in canonical (structural) member order.
    groups: Vec<Vec<usize>>,
}

/// Collect every function name called (directly) anywhere in `expr`. Builtin
/// call names (e.g. `box_new`) are collected too but resolve to no function
/// item, so they add no call-graph edge.
fn collect_call_names(expr: &RawExpr, out: &mut Vec<String>) {
    match expr {
        RawExpr::Call { name, args } => {
            out.push(name.clone());
            for arg in args {
                collect_call_names(arg, out);
            }
        }
        RawExpr::Binary { left, right, .. } => {
            collect_call_names(left, out);
            collect_call_names(right, out);
        }
        RawExpr::Unary { expr, .. } => collect_call_names(expr, out),
        RawExpr::BorrowShared { target, .. } | RawExpr::BorrowMut { target, .. } => {
            collect_call_names(target, out)
        }
        RawExpr::Assign { target, value } => {
            collect_call_names(target, out);
            collect_call_names(value, out);
        }
        RawExpr::Let { value, body, .. } => {
            collect_call_names(value, out);
            collect_call_names(body, out);
        }
        RawExpr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            collect_call_names(cond, out);
            collect_call_names(then_expr, out);
            collect_call_names(else_expr, out);
        }
        RawExpr::Fold {
            target, init, body, ..
        } => {
            collect_call_names(target, out);
            collect_call_names(init, out);
            collect_call_names(body, out);
        }
        RawExpr::Array { elements } => {
            for element in elements {
                collect_call_names(element, out);
            }
        }
        RawExpr::Index { target, index } => {
            collect_call_names(target, out);
            collect_call_names(index, out);
        }
        RawExpr::Record { fields } => {
            for field in fields {
                collect_call_names(&field.value, out);
            }
        }
        RawExpr::FieldAccess { target, .. } => collect_call_names(target, out),
        RawExpr::EnumConstruct { value, .. } => collect_call_names(value, out),
        RawExpr::Case { expr, arms } => {
            collect_call_names(expr, out);
            for arm in arms {
                collect_call_names(&arm.body, out);
            }
        }
        RawExpr::LiteralI64 { .. }
        | RawExpr::LiteralBool { .. }
        | RawExpr::LiteralString { .. }
        | RawExpr::LiteralBytes { .. }
        | RawExpr::Unit
        | RawExpr::ParamRef { .. }
        | RawExpr::ParamName { .. } => {}
    }
}

/// Resolve a call name to a function node: a qualified `module.name` splits at
/// the last `.`; an unqualified name resolves in the caller's module.
fn resolve_call_node(
    name: &str,
    current_module: &str,
    name_to_node: &std::collections::HashMap<(String, String), usize>,
) -> Option<usize> {
    if let Some(dot) = name.rfind('.') {
        let module = &name[..dot];
        let local = &name[dot + 1..];
        name_to_node
            .get(&(module.to_string(), local.to_string()))
            .copied()
    } else {
        name_to_node
            .get(&(current_module.to_string(), name.to_string()))
            .copied()
    }
}

/// Tarjan's strongly-connected-components algorithm. Returns each SCC as a list
/// of node indices. Iterative (explicit work stack) so deep call graphs do not
/// overflow the host stack.
fn tarjan_scc(adjacency: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adjacency.len();
    const UNVISITED: usize = usize::MAX;
    let mut index = vec![UNVISITED; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut scc_stack: Vec<usize> = Vec::new();
    let mut counter = 0usize;
    let mut sccs: Vec<Vec<usize>> = Vec::new();
    // Each work-stack frame is (node, next-neighbor-cursor).
    let mut work: Vec<(usize, usize)> = Vec::new();

    for start in 0..n {
        if index[start] != UNVISITED {
            continue;
        }
        work.push((start, 0));
        while let Some(&(v, cursor)) = work.last() {
            if cursor == 0 {
                index[v] = counter;
                lowlink[v] = counter;
                counter += 1;
                scc_stack.push(v);
                on_stack[v] = true;
            }
            if cursor < adjacency[v].len() {
                let w = adjacency[v][cursor];
                work.last_mut().unwrap().1 += 1;
                if index[w] == UNVISITED {
                    work.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                // All neighbors processed: if v is a root, pop its SCC.
                if lowlink[v] == index[v] {
                    let mut component = Vec::new();
                    loop {
                        let node = scc_stack.pop().unwrap();
                        on_stack[node] = false;
                        component.push(node);
                        if node == v {
                            break;
                        }
                    }
                    sccs.push(component);
                }
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
            }
        }
    }
    sccs
}

/// Rewrite, in a JSON-serialized `RawExpr`, every direct call to an in-clique peer
/// so it carries the peer's current refinement colour instead of its name. This
/// erases member identity (and display names) from the body shape while preserving
/// which structural position calls a peer of which colour — the signal colour
/// refinement folds in. Non-peer calls (builtins, external/non-member functions)
/// keep their names: they are stable identities, not clique members.
fn recolor_peer_calls(
    value: &mut serde_json::Value,
    module: &str,
    name_to_local: &std::collections::HashMap<(String, String), usize>,
    colors: &[String],
) {
    match value {
        serde_json::Value::Object(map) => {
            let peer_tag = if map.get("kind").and_then(|kind| kind.as_str()) == Some("call") {
                map.get("name")
                    .and_then(|name| name.as_str())
                    .and_then(|name| resolve_call_node(name, module, name_to_local))
                    .map(|local| format!("@recursion-peer:{}", colors[local]))
            } else {
                None
            };
            if let Some(tag) = peer_tag {
                map.insert("name".to_string(), serde_json::Value::String(tag));
            }
            for child in map.values_mut() {
                recolor_peer_calls(child, module, name_to_local, colors);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                recolor_peer_calls(item, module, name_to_local, colors);
            }
        }
        _ => {}
    }
}

/// The static (body-independent, name-independent) signature of each clique member:
/// regions, param types, return type, effects. Two members with different signatures
/// never share a colour. No display name appears, so rename stays metadata-only.
fn recursion_member_static_sigs(functions: &[&FunctionSource]) -> Vec<String> {
    functions
        .iter()
        .map(|function| {
            store::canonical_json(&serde_json::json!({
                "regions": function.region_params,
                "params": function.params.iter().map(|param| &param.ty).collect::<Vec<_>>(),
                "return": function.return_type,
                "effects": serde_json::to_value(&function.effects).expect("effects serialize"),
            }))
        })
        .collect()
}

fn distinct_color_count(colors: &[String]) -> usize {
    colors.iter().collect::<std::collections::BTreeSet<_>>().len()
}

/// One run of 1-WL colour refinement to stability: a member's colour folds in the
/// colours of the peers it calls (their identity erased by `recolor_peer_calls`), so
/// the colour reflects the member's structural position. Converges in <= n rounds.
///
/// `preserve_own` controls whether a member's *own* previous colour is folded into
/// its next colour. The initial refinement uses `false`, reproducing the historical
/// colouring exactly (so a clique that 1-WL already discretizes keeps its prior
/// order and content hash). The individualization search uses `true` so that a
/// colour pinned by individualization survives subsequent refinement rounds.
fn refine_recursion_colors(
    functions: &[&FunctionSource],
    static_sig: &[String],
    name_to_local: &std::collections::HashMap<(String, String), usize>,
    initial: &[String],
    preserve_own: bool,
) -> Vec<String> {
    let n = functions.len();
    let mut colors = initial.to_vec();
    let mut classes = distinct_color_count(&colors);
    for _ in 0..n {
        let next: Vec<String> = (0..n)
            .map(|local| {
                let mut body =
                    serde_json::to_value(&functions[local].body).expect("RawExpr serializes");
                recolor_peer_calls(&mut body, &functions[local].module, name_to_local, &colors);
                let payload = if preserve_own {
                    format!(
                        "{}|{}|{}",
                        colors[local],
                        static_sig[local],
                        store::canonical_json(&body)
                    )
                } else {
                    format!("{}|{}", static_sig[local], store::canonical_json(&body))
                };
                store::hash_bytes(b"codedb/recursion-order/v1\0", payload.as_bytes())
            })
            .collect();
        let next_classes = distinct_color_count(&next);
        colors = next;
        if next_classes == classes {
            break;
        }
        classes = next_classes;
    }
    colors
}

/// The clique's canonical *form* under a labeling `order` (where `order[ordinal]`
/// is the local member index assigned that ordinal): each member's body with peer
/// calls recoloured to the peer's ORDINAL, paired with its static signature, in
/// ordinal order. Two labelings are compared by this form to choose the canonical
/// one. The form contains only ordinals (0..n-1) and structure — never member
/// identities or source positions — so isomorphic cliques produce identical forms.
fn recursion_clique_form(
    functions: &[&FunctionSource],
    static_sig: &[String],
    name_to_local: &std::collections::HashMap<(String, String), usize>,
    order: &[usize],
) -> Vec<String> {
    let n = functions.len();
    let mut ordinal_color = vec![String::new(); n];
    for (ordinal, &local) in order.iter().enumerate() {
        ordinal_color[local] = ordinal.to_string();
    }
    order
        .iter()
        .map(|&local| {
            let mut body =
                serde_json::to_value(&functions[local].body).expect("RawExpr serializes");
            recolor_peer_calls(
                &mut body,
                &functions[local].module,
                name_to_local,
                &ordinal_color,
            );
            format!("{}|{}", static_sig[local], store::canonical_json(&body))
        })
        .collect()
}

/// Individualization-refinement search for the canonical labeling of a clique whose
/// 1-WL refinement did not fully discretize. At each node: refine to stability; if
/// discrete, rank members by colour to get a labeling and keep it if its
/// `recursion_clique_form` is the lexicographically smallest seen. Otherwise pick
/// the lowest-coloured non-singleton cell (a structural choice) and recurse once per
/// member, individualizing that member. The result is the lex-min over ALL choices,
/// so it is invariant to the order members were tried in — hence to source order.
/// Members in a genuine automorphism orbit yield equal forms, so the choice among
/// them does not affect the resulting content hash.
fn canonical_label_search(
    functions: &[&FunctionSource],
    static_sig: &[String],
    name_to_local: &std::collections::HashMap<(String, String), usize>,
    seed_colors: Vec<String>,
    depth: usize,
    best: &mut Option<(Vec<String>, Vec<usize>)>,
) {
    let n = functions.len();
    let colors = refine_recursion_colors(functions, static_sig, name_to_local, &seed_colors, true);
    if distinct_color_count(&colors) == n {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| colors[a].cmp(&colors[b]));
        let form = recursion_clique_form(functions, static_sig, name_to_local, &order);
        if best.as_ref().is_none_or(|(best_form, _)| &form < best_form) {
            *best = Some((form, order));
        }
        return;
    }
    // Target cell = the lowest-coloured class with more than one member. Selecting by
    // colour value (not member index) keeps the choice independent of source order.
    let mut cells: std::collections::BTreeMap<&String, Vec<usize>> = Default::default();
    for (local, color) in colors.iter().enumerate() {
        cells.entry(color).or_default().push(local);
    }
    let target_cell = cells
        .into_values()
        .find(|members| members.len() > 1)
        .expect("a non-discrete partition has a non-singleton cell");
    for &member in &target_cell {
        let mut individualized = colors.clone();
        // Pin `member` into its own singleton cell with a distinct, ordering-neutral
        // colour. The depth tag keeps individualizations at different levels apart;
        // the choice of marker cannot affect the result because the final selection
        // is by lex-min form over every branch.
        individualized[member] = format!("\u{0}ind:{depth}\u{0}{}", colors[member]);
        canonical_label_search(
            functions,
            static_sig,
            name_to_local,
            individualized,
            depth + 1,
            best,
        );
    }
}

/// Canonical, name-independent ordering of a recursion clique's member items, so a
/// member's ordinal — and thus its deterministic birth identity (SPEC_V3 §10) — is a
/// property of the clique's STRUCTURE, not of source declaration order. Without it,
/// two textual orderings of one clique receive different content hashes and
/// import→export→import is not a fixpoint.
///
/// 1-WL colour refinement yields the order when it discretizes the clique. But 1-WL
/// is an incomplete graph canonicalization: it can leave distinct (non-automorphic)
/// members sharing a colour — e.g. byte-identical bodies that call peers in
/// position-distinguishable argument slots. Falling back to source order there
/// reintroduces the order-dependence. So when refinement does not discretize, we run
/// individualization-refinement (`canonical_label_search`) to a true canonical
/// labeling instead.
fn canonical_recursion_member_order(
    items: &[ProgramItem],
    member_item_indices: &[usize],
) -> Vec<usize> {
    let n = member_item_indices.len();
    if n <= 1 {
        return member_item_indices.to_vec();
    }
    let functions: Vec<&FunctionSource> = member_item_indices
        .iter()
        .map(|&idx| match &items[idx] {
            ProgramItem::Function(function) => function,
            _ => unreachable!("recursion-group member is not a function"),
        })
        .collect();
    let mut name_to_local: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    for (local, function) in functions.iter().enumerate() {
        name_to_local.insert((function.module.clone(), function.name.clone()), local);
    }
    let static_sig = recursion_member_static_sigs(&functions);

    // Initial 1-WL refinement (own colour NOT folded in), identical to the historical
    // colouring so cliques that already discretize keep their prior order and hash.
    let colors = refine_recursion_colors(
        &functions,
        &static_sig,
        &name_to_local,
        &vec![String::new(); n],
        false,
    );

    let local_order = if distinct_color_count(&colors) == n {
        // Discrete: colour order is canonical (the historical, unchanged path).
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| colors[a].cmp(&colors[b]));
        order
    } else {
        // Not discretized by 1-WL: individualization-refinement to a canonical label.
        let mut best: Option<(Vec<String>, Vec<usize>)> = None;
        canonical_label_search(&functions, &static_sig, &name_to_local, colors, 0, &mut best);
        best.expect("individualization-refinement yields at least one labeling")
            .1
    };

    local_order
        .into_iter()
        .map(|local| member_item_indices[local])
        .collect()
}
