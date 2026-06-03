mod abi;
mod api;
mod artifact;
mod backend;
mod backend_c;
mod branches;
mod build_plan;
mod diff;
mod expr;
mod jobs;
mod link;
mod lowering;
mod migrations;
mod model;
pub mod server;
mod store;
mod types;
mod verify;
pub mod workspace;

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use serde_json::json;

pub use expr::{FunctionSource, RawExpr, Value};
pub use store::CodeDb;
pub use types::ParamSpec;

use backend::ArtifactKind;
use expr::{parse_expr_source, parse_program, parse_signature_source};
use migrations::Operation;
use model::{param_names, preferred_names};

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
pub(crate) const PIPELINE_VERSION: &str = "pipeline:v0";

fn parse_eval_arg(arg: &str, type_name: &str, idx: usize) -> Result<Value> {
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
        let functions = parse_program(&source)?;
        let mut report = String::new();

        for (idx, function) in functions.into_iter().enumerate() {
            let branch = self.branch(MAIN_BRANCH)?;
            let birth_seed = format!("import:{}:{}:{}", function.module, function.name, idx);
            let op = Operation::CreateFunction {
                module: function.module,
                name: function.name,
                birth_seed,
                params: function.params,
                return_type: function.return_type,
                body: function.body,
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
        let symbol = self.resolve_name(&branch.root_hash, "main", function_name)?;
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
            .map(|(idx, (arg, type_hash))| parse_eval_arg(arg, self.type_name(type_hash)?, idx))
            .collect::<Result<Vec<_>>>()?;
        self.eval_name(&branch.root_hash, function_name, parsed_args)
    }

    pub fn emit_c_main_branch(&mut self, function_name: &str) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        self.resolve_name(&branch.root_hash, "main", function_name)
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
        for binding in preferred_names(&root) {
            let symbol = binding.symbol;
            let root_symbol = root
                .symbols
                .iter()
                .find(|entry| entry.symbol == symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {symbol}"))?;
            let signature =
                self.signature_source(&root_symbol.signature, &param_names(&root, &symbol))?;
            out.push_str(&format!(
                "{}.{} {} {}\n",
                binding.module, binding.display_name, symbol, signature
            ));
        }
        Ok(out)
    }

    pub fn list_main_branch_json(&self) -> Result<String> {
        let branch = self.branch(MAIN_BRANCH)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut symbols = Vec::new();
        for binding in preferred_names(&root) {
            let symbol = binding.symbol;
            let root_symbol = root
                .symbols
                .iter()
                .find(|entry| entry.symbol == symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {symbol}"))?;
            symbols.push(json!({
                "module": binding.module,
                "name": binding.display_name,
                "symbol_hash": symbol,
                "signature_hash": root_symbol.signature,
                "definition_hash": root_symbol.definition,
                "signature": self.signature_source(&root_symbol.signature, &param_names(&root, &symbol))?,
            }));
        }
        Ok(format!(
            "{}\n",
            store::canonical_json(&json!({
                "branch": MAIN_BRANCH,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
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
        let body_hash = self.function_body_hash(&root_symbol.definition)?;
        let deps = self.dependencies_for_symbol(&branch.root_hash, &symbol)?;
        let mut out = String::new();
        out.push_str(&format!("symbol {symbol}\n"));
        out.push_str(&format!(
            "name {}.{}\n",
            binding.module, binding.display_name
        ));
        out.push_str(&format!("signature {}\n", root_symbol.signature));
        out.push_str(&format!("definition {}\n", root_symbol.definition));
        out.push_str(&format!("body {body_hash}\n"));
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
        out.push_str(&format!(
            "source fn {}{}\n",
            binding.display_name,
            self.signature_source(&root_symbol.signature, &param_names(&root, &symbol))?
        ));
        out.push_str(&format!(
            "body_source {}\n",
            self.expr_to_source(&body_hash, &root, &param_names(&root, &symbol), 0)?
        ));
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
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self.resolve_symbol_or_name(&branch.root_hash, symbol_or_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let binding = self
            .preferred_binding(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol has no preferred name {symbol}"))?;
        let root_symbol = self
            .root_symbol(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol missing from root {symbol}"))?;
        let body_hash = self.function_body_hash(&root_symbol.definition)?;
        let deps = self.dependencies_for_symbol(&branch.root_hash, &symbol)?;
        let dependencies = deps
            .iter()
            .map(|dep| {
                Ok(json!({
                    "name": self.symbol_display(&root, dep)?,
                    "symbol_hash": dep,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let local_param_names = param_names(&root, &symbol);
        let payload = json!({
            "branch": MAIN_BRANCH,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "symbol_hash": symbol,
            "module": binding.module,
            "name": binding.display_name,
            "signature_hash": root_symbol.signature,
            "definition_hash": root_symbol.definition,
            "body_hash": body_hash,
            "internal_abi_symbol": abi::internal_abi_symbol(&symbol)?,
            "exported_abi_symbols": abi::exported_abi_names(&root, &symbol),
            "signature": self.signature_source(&root_symbol.signature, &local_param_names)?,
            "body_source": self.expr_to_source(&body_hash, &root, &local_param_names, 0)?,
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
        let (params, return_type) = parse_signature_source(signature)?;
        let op = Operation::ChangeFunctionSignature {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            params,
            return_type,
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
