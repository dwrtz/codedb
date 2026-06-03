use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value as JsonValue, json};

use crate::MAIN_BRANCH;
use crate::migrations::Operation;
use crate::model::{ProgramRootPayload, aliases_for, param_names, test_binding_for};
use crate::store::{CodeDb, canonical_json};
use crate::tests::{test_value_from_value, value_from_test_value};

const BLAME_SYMBOL_SCHEMA: &str = "codedb/blame-symbol/v1";
const BLAME_EXPR_SCHEMA: &str = "codedb/blame-expr/v1";
const BISECT_HISTORY_SCHEMA: &str = "codedb/bisect-history/v1";
const WHY_SCHEMA: &str = "codedb/why/v1";

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

    pub fn bisect_history_output_branch_json(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
        expected_output: &str,
    ) -> Result<String> {
        let expected = parse_expected_output_value(expected_output)?;
        let predicate = HistoryPredicate::EvalOutput {
            entry_name: entry_name.to_string(),
            args: args.to_vec(),
            expected,
        };
        let payload = self.bisect_history_branch_value(branch_name, predicate)?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn bisect_history_output_branch(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
        expected_output: &str,
    ) -> Result<String> {
        let payload: JsonValue = serde_json::from_str(
            self.bisect_history_output_branch_json(branch_name, entry_name, args, expected_output)?
                .trim_end(),
        )?;
        Ok(format_bisect_history(&payload))
    }

    pub fn bisect_history_test_branch_json(
        &self,
        branch_name: &str,
        test_name: &str,
        expected_status: &str,
    ) -> Result<String> {
        validate_expected_test_status(expected_status)?;
        let predicate = HistoryPredicate::SemanticTest {
            test_name: test_name.to_string(),
            expected_status: expected_status.to_string(),
        };
        let payload = self.bisect_history_branch_value(branch_name, predicate)?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn bisect_history_test_branch(
        &self,
        branch_name: &str,
        test_name: &str,
        expected_status: &str,
    ) -> Result<String> {
        let payload: JsonValue = serde_json::from_str(
            self.bisect_history_test_branch_json(branch_name, test_name, expected_status)?
                .trim_end(),
        )?;
        Ok(format_bisect_history(&payload))
    }

    fn bisect_history_branch_value(
        &self,
        branch_name: &str,
        predicate: HistoryPredicate,
    ) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let states = self.history_root_states(branch_name)?;
        let mut evaluated = BTreeMap::<usize, JsonValue>::new();

        let mut eval_at = |idx: usize| -> Result<JsonValue> {
            if let Some(cached) = evaluated.get(&idx) {
                return Ok(cached.clone());
            }
            let state = states
                .get(idx)
                .ok_or_else(|| anyhow!("bisect state index out of bounds: {idx}"))?;
            let evaluation = self.evaluate_history_predicate(state, &predicate)?;
            evaluated.insert(idx, evaluation.clone());
            Ok(evaluation)
        };

        let mut status = "predicate_never_matched";
        let mut first_changed = JsonValue::Null;
        let mut previous = JsonValue::Null;
        let mut first_matching = None;
        let final_idx = states.len().saturating_sub(1);
        let final_evaluation = eval_at(final_idx)?;

        if predicate_evaluable(&final_evaluation) {
            let mut low = 0;
            let mut high = final_idx;
            while low < high {
                let mid = (low + high) / 2;
                if predicate_evaluable(&eval_at(mid)?) {
                    high = mid;
                } else {
                    low = mid + 1;
                }
            }
            let first_evaluable = low;
            let first_evaluation = eval_at(first_evaluable)?;
            let final_matches = predicate_matches(&final_evaluation);

            if final_matches {
                if predicate_matches(&first_evaluation) {
                    status = "unchanged";
                    first_matching = Some(first_evaluable);
                } else {
                    status = "changed";
                    let mut low = first_evaluable + 1;
                    let mut high = final_idx;
                    while low < high {
                        let mid = (low + high) / 2;
                        if predicate_matches(&eval_at(mid)?) {
                            high = mid;
                        } else {
                            low = mid + 1;
                        }
                    }
                    first_matching = Some(low);
                    previous = eval_at(low.saturating_sub(1))?;
                    let state = &states[low];
                    first_changed = json!({
                        "sequence": state.sequence,
                        "root_hash": state.root_hash,
                        "history_hash": state.history_hash,
                        "migration": state.migration_from_parent,
                        "previous_evaluation": previous,
                        "changed_evaluation": eval_at(low)?,
                    });
                }
            } else if predicate_matches(&first_evaluation) {
                status = "changed";
                first_matching = Some(first_evaluable);
                let mut low = first_evaluable + 1;
                let mut high = final_idx;
                while low < high {
                    let mid = (low + high) / 2;
                    if !predicate_matches(&eval_at(mid)?) {
                        high = mid;
                    } else {
                        low = mid + 1;
                    }
                }
                previous = eval_at(low.saturating_sub(1))?;
                let state = &states[low];
                first_changed = json!({
                    "sequence": state.sequence,
                    "root_hash": state.root_hash,
                    "history_hash": state.history_hash,
                    "migration": state.migration_from_parent,
                    "previous_evaluation": previous,
                    "changed_evaluation": eval_at(low)?,
                });
            }
        }

        let mut evaluations = evaluated.into_values().collect::<Vec<_>>();
        evaluations.sort_by_key(|value| {
            value
                .get("sequence")
                .and_then(JsonValue::as_u64)
                .unwrap_or(u64::MAX)
        });

        Ok(json!({
            "schema": BISECT_HISTORY_SCHEMA,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "status": status,
            "search_strategy": "binary_search_after_first_evaluable",
            "predicate": predicate.to_json(),
            "root_count": states.len(),
            "first_matching_sequence": first_matching,
            "first_changed": first_changed,
            "previous": previous,
            "evaluations": evaluations,
        }))
    }

    pub fn why_roots_branch_json(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
        from_root: &str,
        to_root: &str,
    ) -> Result<String> {
        let payload =
            self.why_roots_branch_value(branch_name, entry_name, args, from_root, to_root)?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn why_roots_branch(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
        from_root: &str,
        to_root: &str,
    ) -> Result<String> {
        let payload: JsonValue = serde_json::from_str(
            self.why_roots_branch_json(branch_name, entry_name, args, from_root, to_root)?
                .trim_end(),
        )?;
        Ok(format_why(&payload))
    }

    fn why_roots_branch_value(
        &self,
        branch_name: &str,
        entry_name: &str,
        args: &[String],
        from_root: &str,
        to_root: &str,
    ) -> Result<JsonValue> {
        self.load_root(from_root)
            .with_context(|| format!("why --from is not a program root: {from_root}"))?;
        self.load_root(to_root)
            .with_context(|| format!("why --to is not a program root: {to_root}"))?;
        let branch = self.branch(branch_name)?;
        let from_history = self.history_hash_for_root_in_branch(branch_name, from_root)?;
        let to_history = self.history_hash_for_root_in_branch(branch_name, to_root)?;
        let before_trace = serde_json::to_value(self.trace_root_text_args_report(
            from_root,
            from_history.clone(),
            entry_name,
            args,
        )?)?;
        let after_trace = serde_json::to_value(self.trace_root_text_args_report(
            to_root,
            to_history.clone(),
            entry_name,
            args,
        )?)?;
        let diff: JsonValue =
            serde_json::from_str(self.diff_roots_json(from_root, to_root)?.trim_end())?;
        let changed_functions = self.changed_functions_between_roots(from_root, to_root)?;
        let migration_path = self.migration_path_between_roots(branch_name, from_root, to_root)?;
        let direct_migration = if migration_path.len() == 1 {
            migration_path[0].clone()
        } else {
            JsonValue::Null
        };
        let before_result = before_trace
            .get("result")
            .cloned()
            .unwrap_or(JsonValue::Null);
        let after_result = after_trace
            .get("result")
            .cloned()
            .unwrap_or(JsonValue::Null);
        let trace_summary = json!({
            "entry_name": entry_name,
            "args": args,
            "before": {
                "root_hash": from_root,
                "history_hash": from_history,
                "status": before_trace.get("status").cloned().unwrap_or(JsonValue::Null),
                "result": before_result,
            },
            "after": {
                "root_hash": to_root,
                "history_hash": to_history,
                "status": after_trace.get("status").cloned().unwrap_or(JsonValue::Null),
                "result": after_result,
            },
            "result_changed": before_trace.get("result") != after_trace.get("result"),
            "event_count_before": before_trace
                .get("events")
                .and_then(JsonValue::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            "event_count_after": after_trace
                .get("events")
                .and_then(JsonValue::as_array)
                .map(Vec::len)
                .unwrap_or(0),
        });

        Ok(json!({
            "schema": WHY_SCHEMA,
            "branch": branch_name,
            "branch_root_hash": branch.root_hash,
            "branch_history_hash": branch.history_hash,
            "from_root_hash": from_root,
            "to_root_hash": to_root,
            "from_history_hash": from_history,
            "to_history_hash": to_history,
            "entry_name": entry_name,
            "args": args,
            "status": "ok",
            "summary": {
                "result_changed": trace_summary["result_changed"],
                "changed_function_count": changed_functions.len(),
                "migration_count": migration_path.len(),
            },
            "trace_summary": trace_summary,
            "changed_functions": changed_functions,
            "diff": diff,
            "migration_path": migration_path,
            "direct_migration": direct_migration,
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
            }
            | Operation::AddParameter {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    let changed_reasons = self.classify_root_symbol_change_between(
                        &item.input_root,
                        &item.output_root,
                        symbol,
                    )?;
                    if changed_reasons.contains("signature") {
                        reasons.insert("signature");
                    }
                    if changed_reasons.contains("body") {
                        reasons.insert("body");
                    }
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
            Operation::MergeBranch { .. } => {
                let merged_reasons = self.classify_root_symbol_change_between(
                    &item.input_root,
                    &item.output_root,
                    symbol,
                )?;
                if !merged_reasons.is_empty() {
                    reasons.insert("merge");
                    reasons.extend(merged_reasons);
                }
            }
        }
        Ok(reasons.into_iter().collect())
    }

    fn classify_root_symbol_change_between(
        &self,
        old_root_hash: &str,
        new_root_hash: &str,
        symbol: &str,
    ) -> Result<BTreeSet<&'static str>> {
        let old_root = self.load_root(old_root_hash)?;
        let new_root = self.load_root(new_root_hash)?;
        let old_entry = self.root_symbol(&old_root, symbol);
        let new_entry = self.root_symbol(&new_root, symbol);
        let old_names = sorted_symbol_names(&old_root, symbol);
        let new_names = sorted_symbol_names(&new_root, symbol);
        let old_param_names = sorted_symbol_param_names(&old_root, symbol);
        let new_param_names = sorted_symbol_param_names(&new_root, symbol);
        let old_exports = sorted_symbol_exports(&old_root, symbol);
        let new_exports = sorted_symbol_exports(&new_root, symbol);
        let mut reasons = BTreeSet::new();

        match (old_entry, new_entry) {
            (None, Some(_)) => {
                reasons.insert("birth");
                reasons.insert("signature");
                reasons.insert("body");
                if !new_names.is_empty() {
                    reasons.insert("name");
                }
                if !new_exports.is_empty() {
                    reasons.insert("export");
                }
            }
            (Some(_), None) => {
                reasons.insert("delete");
            }
            (Some(old_entry), Some(new_entry)) => {
                if old_entry.signature != new_entry.signature || old_param_names != new_param_names
                {
                    reasons.insert("signature");
                }
                let old_body = self.function_body_hash(&old_entry.definition)?;
                let new_body = self.function_body_hash(&new_entry.definition)?;
                if old_body != new_body {
                    reasons.insert("body");
                }
                if old_names != new_names {
                    reasons.insert("name");
                    if preferred_symbol_name(&old_root, symbol)
                        != preferred_symbol_name(&new_root, symbol)
                    {
                        reasons.insert("rename");
                    }
                }
                if old_exports != new_exports {
                    reasons.insert("export");
                }
            }
            (None, None) => {
                if old_names != new_names {
                    reasons.insert("name");
                }
                if old_exports != new_exports {
                    reasons.insert("export");
                }
            }
        }
        Ok(reasons)
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
        contexts.sort_by_key(json_sort_key);
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

    fn history_root_states(&self, branch_name: &str) -> Result<Vec<HistoryRootState>> {
        let branch = self.branch(branch_name)?;
        let items = self.provenance_history_chain(branch_name)?;
        let mut states = Vec::new();
        if let Some(first) = items.first() {
            states.push(HistoryRootState {
                sequence: 0,
                root_hash: first.input_root.clone(),
                history_hash: first.parent_history_hash.clone(),
                migration_from_parent: JsonValue::Null,
            });
        } else {
            states.push(HistoryRootState {
                sequence: 0,
                root_hash: branch.root_hash,
                history_hash: branch.history_hash,
                migration_from_parent: JsonValue::Null,
            });
            return Ok(states);
        }
        for (idx, item) in items.iter().enumerate() {
            states.push(HistoryRootState {
                sequence: idx + 1,
                root_hash: item.output_root.clone(),
                history_hash: Some(item.history_hash.clone()),
                migration_from_parent: item.to_json_with_reasons(&["history_step"])?,
            });
        }
        Ok(states)
    }

    fn evaluate_history_predicate(
        &self,
        state: &HistoryRootState,
        predicate: &HistoryPredicate,
    ) -> Result<JsonValue> {
        match predicate {
            HistoryPredicate::EvalOutput {
                entry_name,
                args,
                expected,
            } => {
                let trace = self.trace_root_text_args_report(
                    &state.root_hash,
                    state.history_hash.clone(),
                    entry_name,
                    args,
                )?;
                let trace_json = serde_json::to_value(&trace)?;
                let actual = trace_json.get("result").cloned().unwrap_or(JsonValue::Null);
                let matched = trace_json.get("status").and_then(JsonValue::as_str) == Some("ok")
                    && actual == *expected;
                Ok(json!({
                    "sequence": state.sequence,
                    "root_hash": state.root_hash,
                    "history_hash": state.history_hash,
                    "status": trace_json.get("status").cloned().unwrap_or(JsonValue::Null),
                    "matched": matched,
                    "predicate_kind": "eval_output",
                    "entry_name": entry_name,
                    "args": args,
                    "expected": expected,
                    "actual": actual,
                    "diagnostics": trace_json.get("diagnostics").cloned().unwrap_or_else(|| json!([])),
                }))
            }
            HistoryPredicate::SemanticTest {
                test_name,
                expected_status,
            } => {
                let result = self.evaluate_semantic_test_at_root(&state.root_hash, test_name)?;
                let actual_status = result
                    .get("status")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("error");
                Ok(json!({
                    "sequence": state.sequence,
                    "root_hash": state.root_hash,
                    "history_hash": state.history_hash,
                    "status": if actual_status == "error" { "error" } else { "ok" },
                    "matched": actual_status == expected_status,
                    "predicate_kind": "semantic_test",
                    "test_name": test_name,
                    "expected_status": expected_status,
                    "actual_status": actual_status,
                    "test_result": result,
                }))
            }
        }
    }

    fn evaluate_semantic_test_at_root(
        &self,
        root_hash: &str,
        test_name: &str,
    ) -> Result<JsonValue> {
        let root = self.load_root(root_hash)?;
        let Some(binding) = test_binding_for(&root, test_name) else {
            return Ok(json!({
                "status": "error",
                "kind": "test_not_found",
                "message": format!("test {test_name:?} is not present in root {root_hash}"),
            }));
        };
        let case = self.load_test_case(&binding.test)?;
        let expected = value_from_test_value(&case.expected)?;
        let args = case
            .args
            .iter()
            .map(value_from_test_value)
            .collect::<Result<Vec<_>>>()?;
        match self.eval_symbol(root_hash, &case.entry_symbol, args) {
            Ok(actual) => {
                let status = if actual == expected {
                    "passed"
                } else {
                    "failed"
                };
                Ok(json!({
                    "status": status,
                    "name": binding.name,
                    "test_hash": binding.test,
                    "entry_symbol": case.entry_symbol,
                    "entry_name": self.symbol_display(&root, &case.entry_symbol)?,
                    "category": case.category.as_str(),
                    "expected": case.expected,
                    "actual": test_value_from_value(&actual),
                }))
            }
            Err(err) => Ok(json!({
                "status": "error",
                "name": binding.name,
                "test_hash": binding.test,
                "entry_symbol": case.entry_symbol,
                "category": case.category.as_str(),
                "expected": case.expected,
                "error": format!("{err:#}"),
            })),
        }
    }

    fn history_hash_for_root_in_branch(
        &self,
        branch_name: &str,
        root_hash: &str,
    ) -> Result<Option<String>> {
        for state in self.history_root_states(branch_name)? {
            if state.root_hash == root_hash {
                return Ok(state.history_hash);
            }
        }
        Ok(None)
    }

    fn migration_path_between_roots(
        &self,
        branch_name: &str,
        from_root: &str,
        to_root: &str,
    ) -> Result<Vec<JsonValue>> {
        if from_root == to_root {
            return Ok(Vec::new());
        }
        let mut active = false;
        let mut path = Vec::new();
        for item in self.provenance_history_chain(branch_name)? {
            if !active && item.input_root == from_root {
                active = true;
            }
            if active {
                path.push(item.to_json_with_reasons(&["why_path"])?);
                if item.output_root == to_root {
                    return Ok(path);
                }
            }
        }
        Ok(Vec::new())
    }

    fn changed_functions_between_roots(
        &self,
        from_root_hash: &str,
        to_root_hash: &str,
    ) -> Result<Vec<JsonValue>> {
        let from_root = self.load_root(from_root_hash)?;
        let to_root = self.load_root(to_root_hash)?;
        let from_symbols = from_root
            .symbols
            .iter()
            .map(|entry| (entry.symbol.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let to_symbols = to_root
            .symbols
            .iter()
            .map(|entry| (entry.symbol.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let all_symbols = from_symbols
            .keys()
            .chain(to_symbols.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut changed = Vec::new();
        for symbol in all_symbols {
            match (from_symbols.get(&symbol), to_symbols.get(&symbol)) {
                (Some(from_entry), Some(to_entry)) => {
                    let from_body = self.function_body_hash(&from_entry.definition)?;
                    let to_body = self.function_body_hash(&to_entry.definition)?;
                    let mut reasons = Vec::new();
                    if from_entry.signature != to_entry.signature {
                        reasons.push("signature");
                    }
                    if from_body != to_body {
                        reasons.push("body");
                    }
                    let from_name = self.symbol_display(&from_root, &symbol).ok();
                    let to_name = self.symbol_display(&to_root, &symbol).ok();
                    if from_name != to_name {
                        reasons.push("name");
                    }
                    if reasons.is_empty() {
                        continue;
                    }
                    let mut expression_changes = Vec::new();
                    if from_body != to_body {
                        self.collect_expression_changes(
                            &from_root,
                            &to_root,
                            &from_body,
                            &to_body,
                            "body",
                            &mut expression_changes,
                        )?;
                    }
                    changed.push(json!({
                        "kind": "function_changed",
                        "symbol_hash": symbol,
                        "function": to_name.or(from_name).unwrap_or_else(|| symbol.clone()),
                        "reasons": reasons,
                        "from_signature_hash": from_entry.signature,
                        "to_signature_hash": to_entry.signature,
                        "from_definition_hash": from_entry.definition,
                        "to_definition_hash": to_entry.definition,
                        "from_body_hash": from_body,
                        "to_body_hash": to_body,
                        "expression_changes": expression_changes,
                    }));
                }
                (None, Some(to_entry)) => changed.push(json!({
                    "kind": "function_added",
                    "symbol_hash": symbol,
                    "function": self.symbol_display(&to_root, &symbol).unwrap_or_else(|_| symbol.clone()),
                    "to_signature_hash": to_entry.signature,
                    "to_definition_hash": to_entry.definition,
                    "to_body_hash": self.function_body_hash(&to_entry.definition)?,
                })),
                (Some(from_entry), None) => changed.push(json!({
                    "kind": "function_removed",
                    "symbol_hash": symbol,
                    "function": self.symbol_display(&from_root, &symbol).unwrap_or_else(|_| symbol.clone()),
                    "from_signature_hash": from_entry.signature,
                    "from_definition_hash": from_entry.definition,
                    "from_body_hash": self.function_body_hash(&from_entry.definition)?,
                })),
                (None, None) => unreachable!(),
            }
        }
        Ok(changed)
    }

    fn collect_expression_changes(
        &self,
        from_root: &ProgramRootPayload,
        to_root: &ProgramRootPayload,
        from_expr: &str,
        to_expr: &str,
        path: &str,
        changes: &mut Vec<JsonValue>,
    ) -> Result<()> {
        if from_expr == to_expr {
            return Ok(());
        }
        let from_payload = self.get_payload(from_expr)?;
        let to_payload = self.get_payload(to_expr)?;
        let from_kind = from_payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        let to_kind = to_payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        if from_kind != to_kind {
            changes.push(json!({
                "kind": "expression_replaced",
                "path": path,
                "from_expr_hash": from_expr,
                "to_expr_hash": to_expr,
                "from_expr_kind": from_kind,
                "to_expr_kind": to_kind,
            }));
            return Ok(());
        }

        match from_kind {
            "literal_i64" | "literal_bool" => {
                if from_payload.get("value") != to_payload.get("value") {
                    changes.push(json!({
                        "kind": "literal_changed",
                        "path": path,
                        "from_expr_hash": from_expr,
                        "to_expr_hash": to_expr,
                        "expr_kind": from_kind,
                        "from_value": from_payload.get("value").cloned().unwrap_or(JsonValue::Null),
                        "to_value": to_payload.get("value").cloned().unwrap_or(JsonValue::Null),
                    }));
                }
            }
            "literal_unit" => changes.push(json!({
                "kind": "literal_changed",
                "path": path,
                "from_expr_hash": from_expr,
                "to_expr_hash": to_expr,
                "expr_kind": "literal_unit",
            })),
            "call" => {
                let from_symbol = from_payload.get("symbol").and_then(JsonValue::as_str);
                let to_symbol = to_payload.get("symbol").and_then(JsonValue::as_str);
                if from_symbol != to_symbol {
                    changes.push(json!({
                        "kind": "call_target_changed",
                        "path": path,
                        "from_expr_hash": from_expr,
                        "to_expr_hash": to_expr,
                        "from_symbol_hash": from_symbol,
                        "to_symbol_hash": to_symbol,
                        "from_function": from_symbol
                            .map(|symbol| self.symbol_display(from_root, symbol).unwrap_or_else(|_| symbol.to_string())),
                        "to_function": to_symbol
                            .map(|symbol| self.symbol_display(to_root, symbol).unwrap_or_else(|_| symbol.to_string())),
                    }));
                }
                let from_args = json_array_hashes(&from_payload, "args")?;
                let to_args = json_array_hashes(&to_payload, "args")?;
                for (idx, (from_arg, to_arg)) in from_args.iter().zip(to_args.iter()).enumerate() {
                    self.collect_expression_changes(
                        from_root,
                        to_root,
                        from_arg,
                        to_arg,
                        &format!("{path}.args[{idx}]"),
                        changes,
                    )?;
                }
            }
            "binary" => {
                if from_payload.get("op") != to_payload.get("op") {
                    changes.push(json!({
                        "kind": "operator_changed",
                        "path": path,
                        "from_expr_hash": from_expr,
                        "to_expr_hash": to_expr,
                        "from_op": from_payload.get("op").cloned().unwrap_or(JsonValue::Null),
                        "to_op": to_payload.get("op").cloned().unwrap_or(JsonValue::Null),
                    }));
                }
                for key in ["left", "right"] {
                    if let (Some(from_child), Some(to_child)) = (
                        from_payload.get(key).and_then(JsonValue::as_str),
                        to_payload.get(key).and_then(JsonValue::as_str),
                    ) {
                        self.collect_expression_changes(
                            from_root,
                            to_root,
                            from_child,
                            to_child,
                            &format!("{path}.{key}"),
                            changes,
                        )?;
                    }
                }
            }
            "unary" => {
                if from_payload.get("op") != to_payload.get("op") {
                    changes.push(json!({
                        "kind": "operator_changed",
                        "path": path,
                        "from_expr_hash": from_expr,
                        "to_expr_hash": to_expr,
                        "from_op": from_payload.get("op").cloned().unwrap_or(JsonValue::Null),
                        "to_op": to_payload.get("op").cloned().unwrap_or(JsonValue::Null),
                    }));
                }
                if let (Some(from_child), Some(to_child)) = (
                    from_payload.get("expr").and_then(JsonValue::as_str),
                    to_payload.get("expr").and_then(JsonValue::as_str),
                ) {
                    self.collect_expression_changes(
                        from_root,
                        to_root,
                        from_child,
                        to_child,
                        &format!("{path}.expr"),
                        changes,
                    )?;
                }
            }
            "let" => {
                for key in ["binding_name", "binding_type"] {
                    if from_payload.get(key) != to_payload.get(key) {
                        changes.push(json!({
                            "kind": format!("let_{key}_changed"),
                            "path": path,
                            "from_expr_hash": from_expr,
                            "to_expr_hash": to_expr,
                            "from": from_payload.get(key).cloned().unwrap_or(JsonValue::Null),
                            "to": to_payload.get(key).cloned().unwrap_or(JsonValue::Null),
                        }));
                    }
                }
                for key in ["value", "body"] {
                    if let (Some(from_child), Some(to_child)) = (
                        from_payload.get(key).and_then(JsonValue::as_str),
                        to_payload.get(key).and_then(JsonValue::as_str),
                    ) {
                        self.collect_expression_changes(
                            from_root,
                            to_root,
                            from_child,
                            to_child,
                            &format!("{path}.{key}"),
                            changes,
                        )?;
                    }
                }
            }
            "if" => {
                for key in ["cond", "then", "else"] {
                    if let (Some(from_child), Some(to_child)) = (
                        from_payload.get(key).and_then(JsonValue::as_str),
                        to_payload.get(key).and_then(JsonValue::as_str),
                    ) {
                        self.collect_expression_changes(
                            from_root,
                            to_root,
                            from_child,
                            to_child,
                            &format!("{path}.{key}"),
                            changes,
                        )?;
                    }
                }
            }
            "param_ref" | "local_ref" => {
                let key = if from_kind == "param_ref" {
                    "index"
                } else {
                    "depth"
                };
                if from_payload.get(key) != to_payload.get(key) {
                    changes.push(json!({
                        "kind": format!("{from_kind}_changed"),
                        "path": path,
                        "from_expr_hash": from_expr,
                        "to_expr_hash": to_expr,
                        "from": from_payload.get(key).cloned().unwrap_or(JsonValue::Null),
                        "to": to_payload.get(key).cloned().unwrap_or(JsonValue::Null),
                    }));
                }
            }
            _ => changes.push(json!({
                "kind": "expression_changed",
                "path": path,
                "from_expr_hash": from_expr,
                "to_expr_hash": to_expr,
                "expr_kind": from_kind,
            })),
        }
        Ok(())
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

#[derive(Debug, Clone)]
struct HistoryRootState {
    sequence: usize,
    root_hash: String,
    history_hash: Option<String>,
    migration_from_parent: JsonValue,
}

#[derive(Debug, Clone)]
enum HistoryPredicate {
    EvalOutput {
        entry_name: String,
        args: Vec<String>,
        expected: JsonValue,
    },
    SemanticTest {
        test_name: String,
        expected_status: String,
    },
}

impl HistoryPredicate {
    fn to_json(&self) -> JsonValue {
        match self {
            HistoryPredicate::EvalOutput {
                entry_name,
                args,
                expected,
            } => json!({
                "kind": "eval_output",
                "entry_name": entry_name,
                "args": args,
                "expected": expected,
            }),
            HistoryPredicate::SemanticTest {
                test_name,
                expected_status,
            } => json!({
                "kind": "semantic_test",
                "test_name": test_name,
                "expected_status": expected_status,
            }),
        }
    }
}

fn predicate_matches(evaluation: &JsonValue) -> bool {
    evaluation
        .get("matched")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

fn predicate_evaluable(evaluation: &JsonValue) -> bool {
    evaluation.get("status").and_then(JsonValue::as_str) == Some("ok")
}

fn parse_expected_output_value(text: &str) -> Result<JsonValue> {
    if let Some(value) = text.strip_prefix("i64:") {
        value
            .parse::<i64>()
            .with_context(|| format!("expected i64 output must be i64, got {value:?}"))?;
        return Ok(json!({"kind": "i64", "value": value}));
    }
    if let Some(value) = text.strip_prefix("bool:") {
        return match value {
            "true" => Ok(json!({"kind": "bool", "value": true})),
            "false" => Ok(json!({"kind": "bool", "value": false})),
            _ => bail!("expected bool output must be true or false, got {value:?}"),
        };
    }
    if matches!(text, "unit" | "()" | "unit:()") {
        return Ok(json!({"kind": "unit"}));
    }
    match text {
        "true" => Ok(json!({"kind": "bool", "value": true})),
        "false" => Ok(json!({"kind": "bool", "value": false})),
        value => {
            value.parse::<i64>().with_context(|| {
                format!("expected output must be i64, bool, or unit, got {value:?}")
            })?;
            Ok(json!({"kind": "i64", "value": value}))
        }
    }
}

fn validate_expected_test_status(status: &str) -> Result<()> {
    match status {
        "passed" | "failed" | "error" => Ok(()),
        other => bail!("expected test status must be passed, failed, or error, got {other:?}"),
    }
}

fn json_array_hashes(payload: &JsonValue, key: &str) -> Result<Vec<String>> {
    payload
        .get(key)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| anyhow!("{key} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("{key} entries must be hashes"))
        })
        .collect::<Result<Vec<_>>>()
}

fn format_bisect_history(payload: &JsonValue) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "bisect_history {}\n",
        payload
            .get("status")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "branch {}\n",
        payload
            .get("branch")
            .and_then(JsonValue::as_str)
            .unwrap_or(MAIN_BRANCH)
    ));
    if let Some(first_changed) = payload.get("first_changed")
        && !first_changed.is_null()
    {
        let migration = &first_changed["migration"];
        out.push_str(&format!(
            "first_changed_migration {} {}\n",
            migration
                .get("migration_hash")
                .and_then(JsonValue::as_str)
                .unwrap_or("none"),
            migration
                .get("operation_kind")
                .and_then(JsonValue::as_str)
                .unwrap_or("unknown")
        ));
        out.push_str(&format!(
            "from_root {}\n",
            first_changed["previous_evaluation"]
                .get("root_hash")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
        ));
        out.push_str(&format!(
            "to_root {}\n",
            first_changed["changed_evaluation"]
                .get("root_hash")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
        ));
    }
    out
}

fn format_why(payload: &JsonValue) -> String {
    let mut out = String::new();
    out.push_str("why ok\n");
    out.push_str(&format!(
        "from_root {}\n",
        payload
            .get("from_root_hash")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
    ));
    out.push_str(&format!(
        "to_root {}\n",
        payload
            .get("to_root_hash")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
    ));
    let before = &payload["trace_summary"]["before"]["result"];
    let after = &payload["trace_summary"]["after"]["result"];
    out.push_str(&format!(
        "result {} -> {}\n",
        canonical_json(before),
        canonical_json(after)
    ));
    for migration in payload
        .get("migration_path")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        out.push_str(&format!(
            "migration {} {}\n",
            migration
                .get("migration_hash")
                .and_then(JsonValue::as_str)
                .unwrap_or(""),
            migration
                .get("operation_kind")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
        ));
    }
    for function in payload
        .get("changed_functions")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        out.push_str(&format!(
            "changed_function {} {}\n",
            function
                .get("function")
                .and_then(JsonValue::as_str)
                .unwrap_or(""),
            function
                .get("symbol_hash")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
        ));
        for expr in function
            .get("expression_changes")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            out.push_str(&format!(
                "  {} {}\n",
                expr.get("kind")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("expression_changed"),
                expr.get("path").and_then(JsonValue::as_str).unwrap_or("")
            ));
        }
    }
    out
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

fn sorted_symbol_names(root: &ProgramRootPayload, symbol: &str) -> Vec<(String, String, bool)> {
    let mut names = root
        .names
        .iter()
        .filter(|binding| binding.symbol == symbol)
        .map(|binding| {
            (
                binding.module.clone(),
                binding.display_name.clone(),
                binding.is_preferred,
            )
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn sorted_symbol_param_names(root: &ProgramRootPayload, symbol: &str) -> Vec<Vec<String>> {
    let mut param_names = root
        .param_names
        .iter()
        .filter(|binding| binding.symbol == symbol)
        .map(|binding| binding.names.clone())
        .collect::<Vec<_>>();
    param_names.sort();
    param_names
}

fn sorted_symbol_exports(root: &ProgramRootPayload, symbol: &str) -> Vec<String> {
    let mut exports = root
        .exports
        .iter()
        .filter(|binding| binding.symbol == symbol)
        .map(|binding| binding.exported_name.clone())
        .collect::<Vec<_>>();
    exports.sort();
    exports
}

fn preferred_symbol_name(root: &ProgramRootPayload, symbol: &str) -> Option<String> {
    root.names
        .iter()
        .find(|binding| binding.symbol == symbol && binding.is_preferred)
        .map(|binding| binding.display_name.clone())
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
