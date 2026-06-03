use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value as JsonValue, json};

use crate::MAIN_BRANCH;
use crate::migrations::Operation;
use crate::model::{aliases_for, param_names};
use crate::store::{CodeDb, canonical_json};

const BLAME_SYMBOL_SCHEMA: &str = "codedb/blame-symbol/v1";
const BLAME_EXPR_SCHEMA: &str = "codedb/blame-expr/v1";

impl CodeDb {
    pub fn blame_symbol_main_branch_json(&self, symbol_or_name: &str) -> Result<String> {
        self.blame_symbol_branch_json(MAIN_BRANCH, symbol_or_name)
    }

    pub fn blame_symbol_branch_json(&self, branch: &str, symbol_or_name: &str) -> Result<String> {
        let payload = self.blame_symbol_branch_value(branch, symbol_or_name)?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn blame_symbol_main_branch(&self, symbol_or_name: &str) -> Result<String> {
        self.blame_symbol_branch(MAIN_BRANCH, symbol_or_name)
    }

    pub fn blame_symbol_branch(&self, branch: &str, symbol_or_name: &str) -> Result<String> {
        let payload = self.blame_symbol_branch_value(branch, symbol_or_name)?;
        let mut out = String::new();
        out.push_str(&format!(
            "branch {}\n",
            payload["branch"].as_str().unwrap_or(branch)
        ));
        out.push_str(&format!(
            "root {}\n",
            payload["root_hash"].as_str().unwrap_or("")
        ));
        out.push_str(&format!(
            "history {}\n",
            payload["history_hash"].as_str().unwrap_or("none")
        ));
        out.push_str(&format!(
            "symbol {}\n",
            payload["symbol_hash"].as_str().unwrap_or(symbol_or_name)
        ));
        if let Some(name) = payload["name"].as_str() {
            out.push_str(&format!(
                "name {}.{}\n",
                payload["module"].as_str().unwrap_or(MAIN_BRANCH),
                name
            ));
        }
        push_blame_line(&mut out, "birth_migration", &payload["birth_migration"]);
        push_blame_line(
            &mut out,
            "last_signature_migration",
            &payload["last_signature_migration"],
        );
        push_blame_line(
            &mut out,
            "last_body_migration",
            &payload["last_body_migration"],
        );
        push_blame_line(
            &mut out,
            "last_rename_migration",
            &payload["last_rename_migration"],
        );
        push_blame_line(
            &mut out,
            "last_export_migration",
            &payload["last_export_migration"],
        );
        Ok(out)
    }

    fn blame_symbol_branch_value(
        &self,
        branch_name: &str,
        symbol_or_name: &str,
    ) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let symbol = self.resolve_symbol_for_blame(&branch.root_hash, symbol_or_name)?;
        let current_entry = self.root_symbol(&root, &symbol);
        let binding = self.preferred_binding(&root, &symbol);
        let aliases = aliases_for(&root, &symbol).into_iter().collect::<Vec<_>>();
        let exported_names = root
            .exports
            .iter()
            .filter(|export| export.symbol == symbol)
            .map(|export| export.exported_name.clone())
            .collect::<Vec<_>>();
        let definition_hash = current_entry.map(|entry| entry.definition.clone());
        let signature_hash = current_entry.map(|entry| entry.signature.clone());
        let body_hash = definition_hash
            .as_deref()
            .map(|definition| self.function_body_hash(definition))
            .transpose()?;
        let current = current_entry.map(|entry| {
            json!({
                "module": binding.map(|binding| binding.module.as_str()),
                "name": binding.map(|binding| binding.display_name.as_str()),
                "aliases": &aliases,
                "exported_names": &exported_names,
                "signature_hash": entry.signature,
                "definition_hash": entry.definition,
                "body_hash": body_hash.clone(),
            })
        });

        let mut birth = None;
        let mut last_signature = None;
        let mut last_body = None;
        let mut last_name = None;
        let mut last_rename = None;
        let mut last_export = None;
        let mut involved = Vec::new();

        for item in self.provenance_history_chain(branch_name)? {
            let classifications = self.classify_symbol_migration(&item, &symbol)?;
            if classifications.is_empty() {
                continue;
            }
            let record = item.to_json_with_reasons(&classifications)?;
            if classifications.contains(&"birth") {
                birth = Some(record.clone());
            }
            if classifications.contains(&"signature") {
                last_signature = Some(record.clone());
            }
            if classifications.contains(&"body") {
                last_body = Some(record.clone());
            }
            if classifications.contains(&"name") {
                last_name = Some(record.clone());
            }
            if classifications.contains(&"rename") {
                last_rename = Some(record.clone());
            }
            if classifications.contains(&"export") {
                last_export = Some(record.clone());
            }
            involved.push(record);
        }

        Ok(json!({
            "schema": BLAME_SYMBOL_SCHEMA,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "symbol_hash": symbol,
            "module": binding.map(|binding| binding.module.as_str()),
            "name": binding.map(|binding| binding.display_name.as_str()),
            "aliases": aliases,
            "exported_names": exported_names,
            "signature_hash": signature_hash,
            "definition_hash": definition_hash,
            "body_hash": body_hash,
            "current": current,
            "birth_migration": birth,
            "last_signature_migration": last_signature,
            "last_body_migration": last_body,
            "last_name_migration": last_name,
            "last_rename_migration": last_rename,
            "last_export_migration": last_export,
            "involved_migrations": involved,
        }))
    }

    pub fn blame_expr_main_branch_json(&self, expr_hash: &str) -> Result<String> {
        self.blame_expr_branch_json(MAIN_BRANCH, expr_hash)
    }

    pub fn blame_expr_branch_json(&self, branch: &str, expr_hash: &str) -> Result<String> {
        let payload = self.blame_expr_branch_value(branch, expr_hash)?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn blame_expr_main_branch(&self, expr_hash: &str) -> Result<String> {
        self.blame_expr_branch(MAIN_BRANCH, expr_hash)
    }

    pub fn blame_expr_branch(&self, branch: &str, expr_hash: &str) -> Result<String> {
        let payload = self.blame_expr_branch_value(branch, expr_hash)?;
        let mut out = String::new();
        out.push_str(&format!(
            "branch {}\n",
            payload["branch"].as_str().unwrap_or(branch)
        ));
        out.push_str(&format!(
            "root {}\n",
            payload["root_hash"].as_str().unwrap_or("")
        ));
        out.push_str(&format!(
            "history {}\n",
            payload["history_hash"].as_str().unwrap_or("none")
        ));
        out.push_str(&format!("expr {expr_hash}\n"));
        if let Some(expr_kind) = payload["expr_kind"].as_str() {
            out.push_str(&format!("expr_kind {expr_kind}\n"));
        }
        out.push_str(&format!(
            "current_reachable {}\n",
            payload["current_reachable"].as_bool().unwrap_or(false)
        ));
        push_blame_line(
            &mut out,
            "introduced_migration",
            &payload["introduced_migration"],
        );
        Ok(out)
    }

    fn blame_expr_branch_value(&self, branch_name: &str, expr_hash: &str) -> Result<JsonValue> {
        self.ensure_expression_object(expr_hash)?;
        let branch = self.branch(branch_name)?;
        let payload = self.get_payload(expr_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?;
        let type_hash = payload.get("type").and_then(JsonValue::as_str);
        let current_contexts = self.reachable_expr_contexts(&branch.root_hash, expr_hash)?;
        let current_reachable = !current_contexts.is_empty();

        let mut introduced = None;
        let mut last_reachable_change = None;
        let mut involved = Vec::new();
        for item in self.provenance_history_chain(branch_name)? {
            let input_reachable = self.root_reaches_expr(&item.input_root, expr_hash)?;
            let output_reachable = self.root_reaches_expr(&item.output_root, expr_hash)?;
            if input_reachable == output_reachable {
                continue;
            }
            let reason = if output_reachable {
                "introduced"
            } else {
                "removed"
            };
            let record = item.to_json_with_reasons(&[reason])?;
            if output_reachable && introduced.is_none() {
                introduced = Some(record.clone());
            }
            last_reachable_change = Some(record.clone());
            involved.push(record);
        }

        Ok(json!({
            "schema": BLAME_EXPR_SCHEMA,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "expr_hash": expr_hash,
            "expr_kind": expr_kind,
            "type_hash": type_hash,
            "current_reachable": current_reachable,
            "current_contexts": current_contexts,
            "introduced_migration": introduced,
            "last_reachable_change_migration": last_reachable_change,
            "involved_migrations": involved,
        }))
    }

    fn resolve_symbol_for_blame(&self, root_hash: &str, symbol_or_name: &str) -> Result<String> {
        if symbol_or_name.starts_with("sha256:") {
            let kind = self.get_kind(symbol_or_name)?;
            if kind != "SymbolBirth" {
                bail!("object {symbol_or_name} is {kind}, not SymbolBirth");
            }
            return Ok(symbol_or_name.to_string());
        }
        self.resolve_symbol_or_name(root_hash, symbol_or_name)
    }

    fn classify_symbol_migration(
        &self,
        item: &ProvenanceHistoryItem,
        symbol: &str,
    ) -> Result<Vec<&'static str>> {
        let mut reasons = BTreeSet::new();
        match &item.operation {
            Operation::CreateFunction { module, name, .. } => {
                if self
                    .resolve_name(&item.output_root, module, name)
                    .is_ok_and(|created| created == symbol)
                {
                    reasons.insert("birth");
                    reasons.insert("body");
                    reasons.insert("signature");
                    reasons.insert("name");
                }
            }
            Operation::RenameSymbol {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    reasons.insert("name");
                    reasons.insert("rename");
                }
            }
            Operation::ReplaceFunctionBody {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    reasons.insert("body");
                }
            }
            Operation::ChangeFunctionSignature {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    reasons.insert("signature");
                    reasons.insert("body");
                }
            }
            Operation::DeleteSymbol {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    reasons.insert("delete");
                }
            }
            Operation::CreateAlias {
                symbol: changed, ..
            }
            | Operation::RemoveAlias {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    reasons.insert("name");
                }
            }
            Operation::SetExport {
                symbol: changed, ..
            }
            | Operation::RemoveExport {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    reasons.insert("export");
                }
            }
            Operation::CreateTest { entry_symbol, .. } => {
                if entry_symbol == symbol {
                    reasons.insert("test");
                }
            }
            Operation::DeleteTest { .. } => {}
        }
        Ok(reasons.into_iter().collect())
    }

    fn ensure_expression_object(&self, expr_hash: &str) -> Result<()> {
        let kind = self.get_kind(expr_hash)?;
        if kind != "Expression" {
            bail!("object {expr_hash} is {kind}, not Expression");
        }
        Ok(())
    }

    fn root_reaches_expr(&self, root_hash: &str, expr_hash: &str) -> Result<bool> {
        Ok(!self
            .reachable_expr_contexts(root_hash, expr_hash)?
            .is_empty())
    }

    fn reachable_expr_contexts(&self, root_hash: &str, expr_hash: &str) -> Result<Vec<JsonValue>> {
        let root = self.load_root(root_hash)?;
        let mut contexts = Vec::new();
        for entry in &root.symbols {
            let body_hash = self.function_body_hash(&entry.definition)?;
            let mut seen = BTreeSet::new();
            if self.expr_tree_contains(&body_hash, expr_hash, &mut seen)? {
                let binding = self.preferred_binding(&root, &entry.symbol);
                let params = param_names(&root, &entry.symbol);
                contexts.push(json!({
                    "symbol_hash": entry.symbol,
                    "module": binding.map(|binding| binding.module.as_str()),
                    "name": binding.map(|binding| binding.display_name.as_str()),
                    "definition_hash": entry.definition,
                    "body_hash": body_hash,
                    "expr_source": self.expr_to_source(expr_hash, &root, &params, 0).ok(),
                }));
            }
        }
        contexts.sort_by(|a, b| json_sort_key(a).cmp(&json_sort_key(b)));
        Ok(contexts)
    }

    fn expr_tree_contains(
        &self,
        current_hash: &str,
        wanted_hash: &str,
        seen: &mut BTreeSet<String>,
    ) -> Result<bool> {
        if current_hash == wanted_hash {
            return Ok(true);
        }
        if !seen.insert(current_hash.to_string()) {
            return Ok(false);
        }
        let payload = self.get_payload(current_hash)?;
        let expr_kind = payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {current_hash}"))?;
        for child in expression_child_hashes(expr_kind, &payload)? {
            if self.expr_tree_contains(&child, wanted_hash, seen)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn provenance_history_chain(&self, branch_name: &str) -> Result<Vec<ProvenanceHistoryItem>> {
        let state = self.branch(branch_name)?;
        let mut items = Vec::new();
        let mut cursor = state.history_hash;
        let mut seen = BTreeSet::new();
        while let Some(history_hash) = cursor {
            if !seen.insert(history_hash.clone()) {
                bail!("history chain contains a cycle at {history_hash}");
            }
            let item = self
                .conn
                .query_row(
                    "SELECT
                        h.parent_history_hash,
                        h.migration_hash,
                        h.output_root_hash,
                        m.input_root_hash,
                        m.operation_kind,
                        m.operation_json,
                        m.agent_json
                     FROM histories h
                     JOIN migrations m ON m.hash = h.migration_hash
                     WHERE h.history_hash = ?1",
                    params![history_hash],
                    |row| {
                        let parent_history_hash: Option<String> = row.get(0)?;
                        let migration_hash: String = row.get(1)?;
                        let output_root: String = row.get(2)?;
                        let input_root: String = row.get(3)?;
                        let operation_kind: String = row.get(4)?;
                        let operation_json: String = row.get(5)?;
                        let agent_json: String = row.get(6)?;
                        Ok((
                            parent_history_hash,
                            migration_hash,
                            output_root,
                            input_root,
                            operation_kind,
                            operation_json,
                            agent_json,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| anyhow!("missing history {history_hash}"))?;
            let (
                parent_history_hash,
                migration_hash,
                output_root,
                input_root,
                operation_kind,
                operation_json,
                agent_json,
            ) = item;
            let operation: Operation = serde_json::from_str(&operation_json)?;
            items.push(ProvenanceHistoryItem {
                sequence: 0,
                history_hash,
                parent_history_hash: parent_history_hash.clone(),
                migration_hash,
                input_root,
                output_root,
                operation_kind,
                operation,
                agent: serde_json::from_str::<JsonValue>(&agent_json)?,
            });
            cursor = parent_history_hash;
        }
        items.reverse();
        for (sequence, item) in items.iter_mut().enumerate() {
            item.sequence = sequence;
        }
        Ok(items)
    }
}

#[derive(Debug, Clone)]
struct ProvenanceHistoryItem {
    sequence: usize,
    history_hash: String,
    parent_history_hash: Option<String>,
    migration_hash: String,
    input_root: String,
    output_root: String,
    operation_kind: String,
    operation: Operation,
    agent: JsonValue,
}

impl ProvenanceHistoryItem {
    fn to_json_with_reasons(&self, reasons: &[&str]) -> Result<JsonValue> {
        Ok(json!({
            "sequence": self.sequence,
            "migration_hash": self.migration_hash,
            "history_hash": self.history_hash,
            "parent_history_hash": self.parent_history_hash,
            "input_root_hash": self.input_root,
            "output_root_hash": self.output_root,
            "operation_kind": self.operation_kind,
            "operation": serde_json::to_value(&self.operation)?,
            "agent": self.agent,
            "reasons": reasons,
        }))
    }
}

fn push_blame_line(out: &mut String, label: &str, value: &JsonValue) {
    if value.is_null() {
        out.push_str(&format!("{label} none\n"));
        return;
    }
    let migration_hash = value
        .get("migration_hash")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let operation_kind = value
        .get("operation_kind")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    out.push_str(&format!("{label} {migration_hash} {operation_kind}\n"));
}

fn expression_child_hashes(expr_kind: &str, payload: &JsonValue) -> Result<Vec<String>> {
    let mut children = Vec::new();
    match expr_kind {
        "literal_i64" | "literal_bool" | "literal_unit" | "param_ref" | "local_ref" => {}
        "call" => {
            for arg in payload
                .get("args")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| anyhow!("call missing args"))?
            {
                children.push(
                    arg.as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?
                        .to_string(),
                );
            }
        }
        "binary" => {
            for key in ["left", "right"] {
                children.push(
                    payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("binary missing {key}"))?
                        .to_string(),
                );
            }
        }
        "unary" => {
            children.push(
                payload
                    .get("expr")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("unary missing expr"))?
                    .to_string(),
            );
        }
        "let" => {
            for key in ["value", "body"] {
                children.push(
                    payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("let missing {key}"))?
                        .to_string(),
                );
            }
        }
        "if" => {
            for key in ["cond", "then", "else"] {
                children.push(
                    payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("if missing {key}"))?
                        .to_string(),
                );
            }
        }
        other => bail!("unknown expression kind {other}"),
    }
    Ok(children)
}

fn json_sort_key(value: &JsonValue) -> String {
    let mut object = BTreeMap::new();
    if let Some(symbol_hash) = value.get("symbol_hash").and_then(JsonValue::as_str) {
        object.insert("symbol_hash", symbol_hash);
    }
    if let Some(definition_hash) = value.get("definition_hash").and_then(JsonValue::as_str) {
        object.insert("definition_hash", definition_hash);
    }
    format!("{object:?}")
}
