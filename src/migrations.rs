use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::expr::RawExpr;
use crate::model::{
    BranchState, NameBinding, ParamNames, ProgramRootPayload, RootSymbolPayload, param_names,
    root_symbol_index, upsert_param_names,
};
use crate::store::{CodeDb, canonical_json, hash_bytes};
use crate::types::ParamSpec;
use crate::{HISTORY_DOMAIN, MAIN_BRANCH, MIGRATION_DOMAIN};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Operation {
    CreateFunction {
        module: String,
        name: String,
        birth_seed: String,
        params: Vec<ParamSpec>,
        return_type: String,
        body: RawExpr,
    },
    RenameSymbol {
        module: String,
        symbol: String,
        old_name: String,
        new_name: String,
    },
    ReplaceFunctionBody {
        module: String,
        symbol: String,
        name: String,
        body: RawExpr,
    },
    ChangeFunctionSignature {
        module: String,
        symbol: String,
        name: String,
        params: Vec<ParamSpec>,
        return_type: String,
    },
    DeleteSymbol {
        module: String,
        symbol: String,
        name: String,
        force: bool,
    },
    CreateAlias {
        module: String,
        symbol: String,
        name: String,
        alias: String,
    },
}

impl Operation {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Operation::CreateFunction { .. } => "create_function",
            Operation::RenameSymbol { .. } => "rename_symbol",
            Operation::ReplaceFunctionBody { .. } => "replace_function_body",
            Operation::ChangeFunctionSignature { .. } => "change_function_signature",
            Operation::DeleteSymbol { .. } => "delete_symbol",
            Operation::CreateAlias { .. } => "create_alias",
        }
    }
}

#[derive(Debug)]
pub(crate) struct MigrationOutcome {
    pub(crate) old_root: String,
    pub(crate) new_root: String,
    pub(crate) migration_hash: String,
    pub(crate) history_hash: String,
    pub(crate) summary: String,
}

impl CodeDb {
    pub(crate) fn apply_and_record(
        &mut self,
        branch: BranchState,
        op: Operation,
    ) -> Result<MigrationOutcome> {
        let old_root = branch.root_hash.clone();
        let new_root =
            self.apply_operation_to_root(&old_root, branch.history_hash.as_deref(), &op)?;
        let preconditions = self.preconditions_for(&old_root, &op);
        let postconditions = self.postconditions_for(&new_root, &op);
        let operation_json = serde_json::to_value(&op)?;
        let migration_hash = migration_hash(
            branch.history_hash.as_deref(),
            &old_root,
            &new_root,
            &operation_json,
            &preconditions,
            &postconditions,
        );
        let history_hash = history_hash(branch.history_hash.as_deref(), &migration_hash, &new_root);

        self.conn.execute(
            "INSERT OR IGNORE INTO migrations
             (hash, parent_history_hash, input_root_hash, output_root_hash,
              operation_kind, operation_json, preconditions_json, postconditions_json, agent_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '{}')",
            params![
                migration_hash,
                branch.history_hash,
                old_root,
                new_root,
                op.kind_name(),
                canonical_json(&operation_json),
                canonical_json(&preconditions),
                canonical_json(&postconditions),
            ],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO histories
             (history_hash, parent_history_hash, migration_hash, output_root_hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![history_hash, branch.history_hash, migration_hash, new_root],
        )?;
        self.update_branch(MAIN_BRANCH, &new_root, &history_hash)?;
        Ok(MigrationOutcome {
            old_root,
            new_root,
            migration_hash,
            history_hash,
            summary: self.operation_summary(&op),
        })
    }

    pub(crate) fn preconditions_for(&self, input_root: &str, op: &Operation) -> JsonValue {
        match op {
            Operation::CreateFunction { module, name, .. } => json!([
                { "kind": "root_is_current", "root": input_root },
                { "kind": "name_is_available", "module": module, "name": name },
            ]),
            Operation::RenameSymbol {
                module,
                symbol,
                old_name,
                new_name,
            } => json!([
                { "kind": "root_is_current", "root": input_root },
                { "kind": "name_points_to_symbol", "module": module, "name": old_name, "symbol": symbol },
                { "kind": "name_is_available", "module": module, "name": new_name },
            ]),
            Operation::ReplaceFunctionBody {
                module,
                symbol,
                name,
                ..
            }
            | Operation::ChangeFunctionSignature {
                module,
                symbol,
                name,
                ..
            }
            | Operation::DeleteSymbol {
                module,
                symbol,
                name,
                ..
            }
            | Operation::CreateAlias {
                module,
                symbol,
                name,
                ..
            } => json!([
                { "kind": "root_is_current", "root": input_root },
                { "kind": "name_points_to_symbol", "module": module, "name": name, "symbol": symbol },
            ]),
        }
    }

    pub(crate) fn postconditions_for(&self, output_root: &str, op: &Operation) -> JsonValue {
        match op {
            Operation::CreateFunction { module, name, .. } => json!([
                { "kind": "root_exists", "root": output_root },
                { "kind": "name_exists", "module": module, "name": name },
            ]),
            Operation::RenameSymbol {
                module,
                symbol,
                old_name,
                new_name,
            } => json!([
                { "kind": "root_exists", "root": output_root },
                { "kind": "name_points_to_symbol", "module": module, "name": new_name, "symbol": symbol },
                { "kind": "name_absent", "module": module, "name": old_name },
            ]),
            Operation::ReplaceFunctionBody { symbol, .. } => json!([
                { "kind": "root_exists", "root": output_root },
                { "kind": "definition_changed", "symbol": symbol },
            ]),
            Operation::ChangeFunctionSignature { symbol, .. } => json!([
                { "kind": "root_exists", "root": output_root },
                { "kind": "signature_changed", "symbol": symbol },
            ]),
            Operation::DeleteSymbol { symbol, .. } => json!([
                { "kind": "root_exists", "root": output_root },
                { "kind": "symbol_absent", "symbol": symbol },
            ]),
            Operation::CreateAlias {
                module,
                symbol,
                alias,
                ..
            } => json!([
                { "kind": "root_exists", "root": output_root },
                { "kind": "name_points_to_symbol", "module": module, "name": alias, "symbol": symbol },
            ]),
        }
    }

    pub(crate) fn operation_summary(&self, op: &Operation) -> String {
        match op {
            Operation::CreateFunction { module, name, .. } => format!("{module}.{name}"),
            Operation::RenameSymbol {
                module,
                old_name,
                new_name,
                ..
            } => format!("{module}.{old_name} -> {module}.{new_name}"),
            Operation::ReplaceFunctionBody { module, name, .. } => {
                format!("{module}.{name}")
            }
            Operation::ChangeFunctionSignature { module, name, .. } => {
                format!("{module}.{name}")
            }
            Operation::DeleteSymbol { module, name, .. } => format!("{module}.{name}"),
            Operation::CreateAlias {
                module,
                name,
                alias,
                ..
            } => format!("{module}.{name} as {module}.{alias}"),
        }
    }

    pub(crate) fn apply_operation_to_root(
        &mut self,
        input_root: &str,
        parent_history_hash: Option<&str>,
        op: &Operation,
    ) -> Result<String> {
        match op {
            Operation::CreateFunction {
                module,
                name,
                birth_seed,
                params,
                return_type,
                body,
            } => self.apply_create_function(
                input_root,
                parent_history_hash,
                module,
                name,
                birth_seed,
                params,
                return_type,
                body,
            ),
            Operation::RenameSymbol {
                module,
                symbol,
                old_name,
                new_name,
            } => self.apply_rename_symbol(input_root, module, symbol, old_name, new_name),
            Operation::ReplaceFunctionBody {
                module,
                symbol,
                name,
                body,
            } => self.apply_replace_body(input_root, module, symbol, name, body),
            Operation::ChangeFunctionSignature {
                module,
                symbol,
                name,
                params,
                return_type,
            } => self.apply_change_signature(input_root, module, symbol, name, params, return_type),
            Operation::DeleteSymbol {
                module,
                symbol,
                name,
                force,
            } => self.apply_delete_symbol(input_root, module, symbol, name, *force),
            Operation::CreateAlias {
                module,
                symbol,
                name,
                alias,
            } => self.apply_create_alias(input_root, module, symbol, name, alias),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_create_function(
        &mut self,
        input_root: &str,
        parent_history_hash: Option<&str>,
        module: &str,
        name: &str,
        birth_seed: &str,
        params: &[ParamSpec],
        return_type: &str,
        body: &RawExpr,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        if root
            .names
            .iter()
            .any(|binding| binding.module == module && binding.display_name == name)
        {
            bail!("name already exists: {module}.{name}");
        }

        let symbol = self.put_symbol_birth(parent_history_hash, birth_seed)?;
        let param_types = params
            .iter()
            .map(|param| self.resolve_type(&param.ty))
            .collect::<Result<Vec<_>>>()?;
        let return_type_hash = self.resolve_type(return_type)?;
        let signature = self.put_signature(&param_types, &return_type_hash)?;
        let param_name_list = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        let typed_body = self.type_expr(body, &root, &param_name_list, &param_types)?;
        if typed_body.type_hash != return_type_hash {
            bail!(
                "function {module}.{name} body type {} does not match return type {}",
                self.type_name(&typed_body.type_hash)?,
                self.type_name(&return_type_hash)?
            );
        }
        let definition = self.put_function_def(&symbol, &signature, &typed_body.expr_hash)?;

        root.symbols.push(RootSymbolPayload {
            symbol: symbol.clone(),
            definition,
            signature: signature.clone(),
        });
        root.names.push(NameBinding {
            module: module.to_string(),
            display_name: name.to_string(),
            symbol: symbol.clone(),
            is_preferred: true,
        });
        root.param_names.push(ParamNames {
            symbol,
            names: param_name_list,
        });
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_rename_symbol(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        if root
            .names
            .iter()
            .any(|binding| binding.module == module && binding.display_name == new_name)
        {
            bail!("name already exists: {module}.{new_name}");
        }
        let mut changed = false;
        for binding in &mut root.names {
            if binding.module == module
                && binding.display_name == old_name
                && binding.symbol == symbol
                && binding.is_preferred
            {
                binding.display_name = new_name.to_string();
                changed = true;
            }
        }
        if !changed {
            bail!("precondition failed: {module}.{old_name} does not point to {symbol}");
        }
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_replace_body(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        body: &RawExpr,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let idx = root_symbol_index(&root, symbol)?;
        let signature = root.symbols[idx].signature.clone();
        let (param_types, return_type) = self.signature_parts(&signature)?;
        let param_name_list = param_names(&root, symbol);
        let typed_body = self.type_expr(body, &root, &param_name_list, &param_types)?;
        if typed_body.type_hash != return_type {
            bail!(
                "replacement body type {} does not match return type {}",
                self.type_name(&typed_body.type_hash)?,
                self.type_name(&return_type)?
            );
        }
        let definition = self.put_function_def(symbol, &signature, &typed_body.expr_hash)?;
        root.symbols[idx].definition = definition;
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_change_signature(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        params: &[ParamSpec],
        return_type: &str,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let idx = root_symbol_index(&root, symbol)?;
        let old_definition = root.symbols[idx].definition.clone();
        let old_body_hash = self.function_body_hash(&old_definition)?;
        let raw_body = self.typed_expr_to_raw(&old_body_hash, &root)?;
        let param_types = params
            .iter()
            .map(|param| self.resolve_type(&param.ty))
            .collect::<Result<Vec<_>>>()?;
        let return_type_hash = self.resolve_type(return_type)?;
        let signature = self.put_signature(&param_types, &return_type_hash)?;
        let param_name_list = params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        let typed_body = self.type_expr(&raw_body, &root, &param_name_list, &param_types)?;
        if typed_body.type_hash != return_type_hash {
            bail!(
                "body type {} does not match new return type {}",
                self.type_name(&typed_body.type_hash)?,
                self.type_name(&return_type_hash)?
            );
        }
        let definition = self.put_function_def(symbol, &signature, &typed_body.expr_hash)?;
        root.symbols[idx].signature = signature;
        root.symbols[idx].definition = definition;
        upsert_param_names(&mut root, symbol, param_name_list);
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)
            .context("new signature invalidates existing root")?;
        Ok(new_root)
    }

    pub(crate) fn apply_delete_symbol(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        force: bool,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        let deps = self.reverse_dependencies_for_root(&root, symbol)?;
        if !force && !deps.is_empty() {
            let names = deps
                .into_iter()
                .map(|dep| self.symbol_display(&root, &dep))
                .collect::<Result<Vec<_>>>()?;
            bail!(
                "cannot delete {module}.{name}; live callers: {}",
                names.join(", ")
            );
        }
        root.symbols.retain(|entry| entry.symbol != symbol);
        root.names.retain(|binding| binding.symbol != symbol);
        root.param_names.retain(|entry| entry.symbol != symbol);
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        self.type_check_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn apply_create_alias(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        alias: &str,
    ) -> Result<String> {
        let mut root = self.load_root(input_root)?;
        self.assert_name_points(&root, module, name, symbol)?;
        if root
            .names
            .iter()
            .any(|binding| binding.module == module && binding.display_name == alias)
        {
            bail!("name already exists: {module}.{alias}");
        }
        root.names.push(NameBinding {
            module: module.to_string(),
            display_name: alias.to_string(),
            symbol: symbol.to_string(),
            is_preferred: false,
        });
        let new_root = self.put_program_root(&root)?;
        self.index_root(&new_root)?;
        Ok(new_root)
    }

    pub(crate) fn assert_name_points(
        &self,
        root: &ProgramRootPayload,
        module: &str,
        name: &str,
        symbol: &str,
    ) -> Result<()> {
        if root.names.iter().any(|binding| {
            binding.module == module && binding.display_name == name && binding.symbol == symbol
        }) {
            Ok(())
        } else {
            bail!("precondition failed: {module}.{name} does not point to {symbol}")
        }
    }

    pub fn history_main_branch(&self) -> Result<String> {
        let chain = self.history_chain(MAIN_BRANCH)?;
        let mut out = String::new();
        for item in chain {
            out.push_str(&format!(
                "{} {} -> {}\n  migration {}\n  history {}\n",
                item.operation_kind,
                item.input_root,
                item.output_root,
                item.migration_hash,
                item.history_hash
            ));
        }
        if out.is_empty() {
            out.push_str("history empty\n");
        }
        Ok(out)
    }

    pub fn replay_main_branch(&mut self) -> Result<String> {
        self.ensure_initialized()?;
        let expected = self.branch(MAIN_BRANCH)?;
        let chain = self.history_chain(MAIN_BRANCH)?;
        let mut current_root = self.put_program_root(&ProgramRootPayload {
            symbols: vec![],
            names: vec![],
            param_names: vec![],
            metadata: BTreeMap::new(),
        })?;
        let mut current_history: Option<String> = None;

        for item in &chain {
            if item.input_root != current_root {
                bail!(
                    "bad_history_link: migration {} expected input {}, replay has {}",
                    item.migration_hash,
                    item.input_root,
                    current_root
                );
            }
            let produced = self.apply_operation_to_root(
                &current_root,
                current_history.as_deref(),
                &item.operation,
            )?;
            if produced != item.output_root {
                bail!(
                    "replay mismatch for {}: expected {}, produced {}",
                    item.migration_hash,
                    item.output_root,
                    produced
                );
            }
            let recomputed_history =
                history_hash(current_history.as_deref(), &item.migration_hash, &produced);
            if recomputed_history != item.history_hash {
                bail!(
                    "bad_history_link: expected history {}, recomputed {}",
                    item.history_hash,
                    recomputed_history
                );
            }
            current_root = produced;
            current_history = Some(recomputed_history);
        }

        if current_root != expected.root_hash {
            bail!(
                "replay final root mismatch: expected {}, replayed {}",
                expected.root_hash,
                current_root
            );
        }
        if current_history != expected.history_hash {
            bail!(
                "replay final history mismatch: expected {:?}, replayed {:?}",
                expected.history_hash,
                current_history
            );
        }
        Ok(format!(
            "replay ok\nroot {}\nhistory {}\n",
            current_root,
            current_history.unwrap_or_else(|| "none".to_string())
        ))
    }

    fn history_chain(&self, branch: &str) -> Result<Vec<HistoryItem>> {
        let state = self.branch(branch)?;
        let mut items = Vec::new();
        let mut cursor = state.history_hash;
        while let Some(history_hash_value) = cursor {
            let (parent_history, migration_hash_value, output_root): (
                Option<String>,
                String,
                String,
            ) = self.conn.query_row(
                "SELECT parent_history_hash, migration_hash, output_root_hash
                 FROM histories WHERE history_hash = ?1",
                params![history_hash_value],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let (input_root, operation_kind, operation_json): (String, String, String) =
                self.conn.query_row(
                    "SELECT input_root_hash, operation_kind, operation_json
                     FROM migrations WHERE hash = ?1",
                    params![migration_hash_value],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
            let operation: Operation = serde_json::from_str(&operation_json)?;
            items.push(HistoryItem {
                history_hash: history_hash_value,
                migration_hash: migration_hash_value,
                input_root,
                output_root,
                operation_kind,
                operation,
            });
            cursor = parent_history;
        }
        items.reverse();
        Ok(items)
    }
}

#[derive(Debug)]
struct HistoryItem {
    history_hash: String,
    migration_hash: String,
    input_root: String,
    output_root: String,
    operation_kind: String,
    operation: Operation,
}

pub(crate) fn migration_hash(
    parent_history_hash: Option<&str>,
    input_root_hash: &str,
    output_root_hash: &str,
    operation: &JsonValue,
    preconditions: &JsonValue,
    postconditions: &JsonValue,
) -> String {
    let payload = json!({
        "parent_history_hash": parent_history_hash,
        "input_root_hash": input_root_hash,
        "output_root_hash": output_root_hash,
        "operation": operation,
        "preconditions": preconditions,
        "postconditions": postconditions,
    });
    hash_bytes(MIGRATION_DOMAIN, canonical_json(&payload).as_bytes())
}

pub(crate) fn history_hash(
    parent_history_hash: Option<&str>,
    migration_hash: &str,
    output_root_hash: &str,
) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(parent_history_hash.unwrap_or("").as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(migration_hash.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(output_root_hash.as_bytes());
    hash_bytes(HISTORY_DOMAIN, &bytes)
}
