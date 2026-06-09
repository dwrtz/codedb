use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value as JsonValue, json};

use crate::MAIN_BRANCH;
use crate::expr::parse_expr_source;
use crate::migrations::Operation;
use crate::model::{ProgramRootPayload, aliases_for, param_names, test_binding_for};
use crate::store::{CodeDb, canonical_json};
use crate::tests::{test_value_from_value, value_from_test_value};
use crate::types::{Effect, TypeDefinition, TypeSpec, type_payload_for_spec};

const BLAME_SYMBOL_SCHEMA: &str = "codedb/blame-symbol/v1";
const BLAME_EXPR_SCHEMA: &str = "codedb/blame-expr/v1";
const BLAME_TYPE_SCHEMA: &str = "codedb/blame-type/v1";
const BLAME_FIELD_SCHEMA: &str = "codedb/blame-field/v1";
const BLAME_VARIANT_SCHEMA: &str = "codedb/blame-variant/v1";
const BISECT_HISTORY_SCHEMA: &str = "codedb/bisect-history/v1";
const WHY_SCHEMA: &str = "codedb/why/v1";
const WHY_BORROW_SCHEMA: &str = "codedb/why-borrow/v1";
const WHY_MOVE_SCHEMA: &str = "codedb/why-move/v1";
const WHY_DROP_SCHEMA: &str = "codedb/why-drop/v1";
const WHY_LAYOUT_SCHEMA: &str = "codedb/why-layout/v1";
const WHY_EFFECT_SCHEMA: &str = "codedb/why-effect/v1";
const WHY_PLATFORM_EXTERN_SCHEMA: &str = "codedb/why-platform-extern/v1";

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
        let body_hash = match definition_hash.as_deref() {
            Some(definition) if !self.definition_is_external(definition)? => {
                Some(self.function_body_hash(definition)?)
            }
            _ => None,
        };
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

    pub fn blame_type_main_branch_json(&self, type_or_name: &str) -> Result<String> {
        self.blame_type_branch_json(MAIN_BRANCH, type_or_name)
    }

    pub fn blame_type_branch_json(&self, branch: &str, type_or_name: &str) -> Result<String> {
        Ok(format!(
            "{}\n",
            canonical_json(&self.blame_type_branch_value(branch, type_or_name)?)
        ))
    }

    pub fn blame_type_main_branch(&self, type_or_name: &str) -> Result<String> {
        self.blame_type_branch(MAIN_BRANCH, type_or_name)
    }

    pub fn blame_type_branch(&self, branch: &str, type_or_name: &str) -> Result<String> {
        let payload = self.blame_type_branch_value(branch, type_or_name)?;
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
            "type {}\n",
            payload["type_symbol"].as_str().unwrap_or(type_or_name)
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
            "last_definition_migration",
            &payload["last_definition_migration"],
        );
        push_blame_line(
            &mut out,
            "last_name_migration",
            &payload["last_name_migration"],
        );
        push_blame_line(
            &mut out,
            "last_rename_migration",
            &payload["last_rename_migration"],
        );
        Ok(out)
    }

    fn blame_type_branch_value(&self, branch_name: &str, type_or_name: &str) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let type_symbol = self.resolve_type_for_blame(&branch.root_hash, type_or_name)?;
        let binding = self.preferred_type_binding(&root, &type_symbol);
        let current_entry = self.root_type(&root, &type_symbol);
        let type_def_hash = current_entry.map(|entry| entry.type_def.clone());

        let mut birth = None;
        let mut last_definition = None;
        let mut last_name = None;
        let mut last_rename = None;
        let mut involved = Vec::new();
        for item in self.provenance_history_chain(branch_name)? {
            let classifications = self.classify_type_migration(&item, &type_symbol)?;
            if classifications.is_empty() {
                continue;
            }
            let record = item.to_json_with_reasons(&classifications)?;
            if classifications.contains(&"birth") {
                birth = Some(record.clone());
            }
            if classifications.contains(&"definition") {
                last_definition = Some(record.clone());
            }
            if classifications.contains(&"name") {
                last_name = Some(record.clone());
            }
            if classifications.contains(&"rename") {
                last_rename = Some(record.clone());
            }
            involved.push(record);
        }

        Ok(json!({
            "schema": BLAME_TYPE_SCHEMA,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "type_symbol": type_symbol,
            "module": binding.map(|binding| binding.module.as_str()),
            "name": binding.map(|binding| binding.display_name.as_str()),
            "type_def_hash": type_def_hash,
            "birth_migration": birth,
            "last_definition_migration": last_definition,
            "last_name_migration": last_name,
            "last_rename_migration": last_rename,
            "involved_migrations": involved,
        }))
    }

    pub fn blame_field_main_branch_json(&self, type_or_name: &str, field: &str) -> Result<String> {
        self.blame_field_branch_json(MAIN_BRANCH, type_or_name, field)
    }

    pub fn blame_field_branch_json(
        &self,
        branch: &str,
        type_or_name: &str,
        field: &str,
    ) -> Result<String> {
        Ok(format!(
            "{}\n",
            canonical_json(&self.blame_member_branch_value(branch, type_or_name, field, true)?)
        ))
    }

    pub fn blame_field_main_branch(&self, type_or_name: &str, field: &str) -> Result<String> {
        self.blame_field_branch(MAIN_BRANCH, type_or_name, field)
    }

    pub fn blame_field_branch(
        &self,
        branch: &str,
        type_or_name: &str,
        field: &str,
    ) -> Result<String> {
        self.blame_member_branch(branch, type_or_name, field, true)
    }

    pub fn blame_variant_main_branch_json(
        &self,
        type_or_name: &str,
        variant: &str,
    ) -> Result<String> {
        self.blame_variant_branch_json(MAIN_BRANCH, type_or_name, variant)
    }

    pub fn blame_variant_branch_json(
        &self,
        branch: &str,
        type_or_name: &str,
        variant: &str,
    ) -> Result<String> {
        Ok(format!(
            "{}\n",
            canonical_json(&self.blame_member_branch_value(
                branch,
                type_or_name,
                variant,
                false
            )?)
        ))
    }

    pub fn blame_variant_main_branch(&self, type_or_name: &str, variant: &str) -> Result<String> {
        self.blame_variant_branch(MAIN_BRANCH, type_or_name, variant)
    }

    pub fn blame_variant_branch(
        &self,
        branch: &str,
        type_or_name: &str,
        variant: &str,
    ) -> Result<String> {
        self.blame_member_branch(branch, type_or_name, variant, false)
    }

    fn blame_member_branch(
        &self,
        branch: &str,
        type_or_name: &str,
        member: &str,
        is_field: bool,
    ) -> Result<String> {
        let payload = self.blame_member_branch_value(branch, type_or_name, member, is_field)?;
        let kind = if is_field { "field" } else { "variant" };
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
        if let Some(type_name) = payload["type_name"].as_str() {
            out.push_str(&format!(
                "type {}.{}\n",
                payload["type_module"].as_str().unwrap_or(MAIN_BRANCH),
                type_name
            ));
        }
        out.push_str(&format!(
            "{kind} {}\n",
            payload["member_symbol"].as_str().unwrap_or(member)
        ));
        if let Some(name) = payload["name"].as_str() {
            out.push_str(&format!("name {name}\n"));
        }
        push_blame_line(&mut out, "birth_migration", &payload["birth_migration"]);
        push_blame_line(
            &mut out,
            "last_name_migration",
            &payload["last_name_migration"],
        );
        push_blame_line(
            &mut out,
            "last_rename_migration",
            &payload["last_rename_migration"],
        );
        push_blame_line(
            &mut out,
            "last_remove_migration",
            &payload["last_remove_migration"],
        );
        Ok(out)
    }

    fn blame_member_branch_value(
        &self,
        branch_name: &str,
        type_or_name: &str,
        member: &str,
        is_field: bool,
    ) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let type_symbol = self.resolve_type_for_blame(&branch.root_hash, type_or_name)?;
        let member_symbol = self.resolve_member_for_blame(&root, &type_symbol, member, is_field)?;
        let type_binding = self.preferred_type_binding(&root, &type_symbol);

        let mut birth = None;
        let mut last_name = None;
        let mut last_rename = None;
        let mut last_remove = None;
        let mut involved = Vec::new();
        for item in self.provenance_history_chain(branch_name)? {
            let classifications =
                self.classify_member_migration(&item, &type_symbol, &member_symbol, is_field)?;
            if classifications.is_empty() {
                continue;
            }
            let record = item.to_json_with_reasons(&classifications)?;
            if classifications.contains(&"birth") {
                birth = Some(record.clone());
            }
            if classifications.contains(&"name") {
                last_name = Some(record.clone());
            }
            if classifications.contains(&"rename") {
                last_rename = Some(record.clone());
            }
            if classifications.contains(&"remove") {
                last_remove = Some(record.clone());
            }
            involved.push(record);
        }

        let schema = if is_field {
            BLAME_FIELD_SCHEMA
        } else {
            BLAME_VARIANT_SCHEMA
        };
        Ok(json!({
            "schema": schema,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "type_symbol": type_symbol,
            "type_module": type_binding.map(|binding| binding.module.as_str()),
            "type_name": type_binding.map(|binding| binding.display_name.as_str()),
            "member_symbol": member_symbol,
            "birth_migration": birth,
            "last_name_migration": last_name,
            "last_rename_migration": last_rename,
            "last_remove_migration": last_remove,
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
        let mut search_strategy = "linear_transition_scan_after_first_evaluable";
        let mut first_changed = JsonValue::Null;
        let mut previous = JsonValue::Null;
        let mut first_matching = None;
        let mut first_evaluable = None::<(usize, bool, JsonValue)>;

        for (idx, _) in states.iter().enumerate() {
            let evaluation = eval_at(idx)?;
            if predicate_evaluable(&evaluation) {
                let matched = predicate_matches(&evaluation);
                first_evaluable = Some((idx, matched, evaluation));
                break;
            }
        }

        let mut scan_end = states.len().saturating_sub(1);
        if let Some((first_idx, first_matched, first_evaluation)) = &first_evaluable {
            if *first_matched {
                first_matching = Some(*first_idx);
            }
            let mut last_evaluable = Some((*first_idx, *first_matched, first_evaluation.clone()));

            for (idx, _) in states.iter().enumerate().skip(first_idx + 1).rev() {
                let evaluation = eval_at(idx)?;
                if predicate_evaluable(&evaluation) {
                    last_evaluable = Some((idx, predicate_matches(&evaluation), evaluation));
                    break;
                }
            }

            if let Some((last_idx, last_matched, _)) = &last_evaluable
                && *last_idx > *first_idx
                && *last_matched != *first_matched
            {
                let mut low = first_idx + 1;
                let mut high = *last_idx;
                let mut binary_ok = true;
                while low < high {
                    let mid = low + (high - low) / 2;
                    let evaluation = eval_at(mid)?;
                    if !predicate_evaluable(&evaluation) {
                        binary_ok = false;
                        break;
                    }
                    if predicate_matches(&evaluation) == *first_matched {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                if binary_ok {
                    let candidate = eval_at(low)?;
                    if predicate_evaluable(&candidate)
                        && predicate_matches(&candidate) != *first_matched
                    {
                        scan_end = low;
                        search_strategy = "binary_transition_search_with_prefix_verification";
                    }
                }
            }

            let mut previous_evaluable =
                Some((*first_idx, *first_matched, first_evaluation.clone()));
            for (idx, _) in states
                .iter()
                .enumerate()
                .skip(first_idx + 1)
                .take(scan_end.saturating_sub(*first_idx))
            {
                let evaluation = eval_at(idx)?;
                if !predicate_evaluable(&evaluation) {
                    continue;
                }

                let matched = predicate_matches(&evaluation);
                if matched && first_matching.is_none() {
                    first_matching = Some(idx);
                }

                if let Some((_, previous_matched, previous_evaluation)) = &previous_evaluable
                    && matched != *previous_matched
                {
                    status = "changed";
                    previous = previous_evaluation.clone();
                    let state = &states[idx];
                    first_changed = json!({
                        "sequence": state.sequence,
                        "root_hash": state.root_hash,
                        "history_hash": state.history_hash,
                        "migration": state.migration_from_parent,
                        "previous_evaluation": previous,
                        "changed_evaluation": evaluation,
                    });
                    break;
                }

                previous_evaluable = Some((idx, matched, evaluation));
            }

            if status != "changed"
                && let Some((_, matched, _)) = last_evaluable
                && matched
            {
                status = "unchanged";
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
            "search_strategy": search_strategy,
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
        let migration_span = self.migration_span_between_roots(branch_name, from_root, to_root)?;
        let from_history = migration_span.from_history_hash.clone();
        let to_history = migration_span.to_history_hash.clone();
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
        let migration_path = migration_span.migrations;
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

    pub fn why_layout_branch_json(
        &mut self,
        branch_name: &str,
        type_or_name: &str,
        field: Option<&str>,
        target_triple: &str,
    ) -> Result<String> {
        let payload =
            self.why_layout_branch_value(branch_name, type_or_name, field, target_triple)?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn why_layout_branch(
        &mut self,
        branch_name: &str,
        type_or_name: &str,
        field: Option<&str>,
        target_triple: &str,
    ) -> Result<String> {
        let payload =
            self.why_layout_branch_value(branch_name, type_or_name, field, target_triple)?;
        Ok(format_v2_why(&payload))
    }

    fn why_layout_branch_value(
        &mut self,
        branch_name: &str,
        type_or_name: &str,
        field: Option<&str>,
        target_triple: &str,
    ) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let resolved = self.resolve_type_for_why(&root, type_or_name)?;
        let layout = self
            .compute_type_layout(&root, &resolved.type_hash, target_triple)?
            .metadata;
        let field_layout = match field {
            Some(field_name) => Some(layout_field_by_name_or_symbol(&layout, field_name)?),
            None => None,
        };
        let field_blame = match (&resolved.type_symbol, field) {
            (Some(type_symbol), Some(field_name)) => {
                Some(self.blame_member_branch_value(branch_name, type_symbol, field_name, true)?)
            }
            _ => None,
        };
        let type_blame = match &resolved.type_symbol {
            Some(type_symbol) => Some(self.blame_type_branch_value(branch_name, type_symbol)?),
            None => None,
        };
        Ok(json!({
            "schema": WHY_LAYOUT_SCHEMA,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "target_triple": target_triple,
            "query": {
                "type": type_or_name,
                "field": field,
            },
            "type_hash": resolved.type_hash,
            "type_symbol": resolved.type_symbol,
            "type_def_hash": resolved.type_def_hash,
            "layout": layout,
            "field_layout": field_layout,
            "explanation": layout_explanation(field, &field_layout),
            "type_blame": type_blame,
            "field_blame": field_blame,
        }))
    }

    pub fn why_drop_branch_json(
        &mut self,
        branch_name: &str,
        type_or_name: &str,
        target_triple: &str,
    ) -> Result<String> {
        let payload = self.why_drop_branch_value(branch_name, type_or_name, target_triple)?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn why_drop_branch(
        &mut self,
        branch_name: &str,
        type_or_name: &str,
        target_triple: &str,
    ) -> Result<String> {
        let payload = self.why_drop_branch_value(branch_name, type_or_name, target_triple)?;
        Ok(format_v2_why(&payload))
    }

    fn why_drop_branch_value(
        &mut self,
        branch_name: &str,
        type_or_name: &str,
        target_triple: &str,
    ) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let resolved = self.resolve_type_for_why(&root, type_or_name)?;
        let layout = self
            .compute_type_layout(&root, &resolved.type_hash, target_triple)?
            .metadata;
        let type_blame = match &resolved.type_symbol {
            Some(type_symbol) => Some(self.blame_type_branch_value(branch_name, type_symbol)?),
            None => None,
        };
        Ok(json!({
            "schema": WHY_DROP_SCHEMA,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "target_triple": target_triple,
            "query": {
                "type": type_or_name,
            },
            "type_hash": resolved.type_hash,
            "type_symbol": resolved.type_symbol,
            "type_def_hash": resolved.type_def_hash,
            "copy_kind": layout.get("copy_kind").cloned().unwrap_or(JsonValue::Null),
            "drop_kind": layout.get("drop_kind").cloned().unwrap_or(JsonValue::Null),
            "contains_reference": layout.get("contains_reference").cloned().unwrap_or(JsonValue::Null),
            "contains_mut_reference": layout.get("contains_mut_reference").cloned().unwrap_or(JsonValue::Null),
            "contains_box": layout.get("contains_box").cloned().unwrap_or(JsonValue::Null),
            "contains_owned_resource": layout.get("contains_owned_resource").cloned().unwrap_or(JsonValue::Null),
            "layout": layout,
            "reasons": drop_reasons(&layout),
            "type_blame": type_blame,
        }))
    }

    pub fn why_effect_branch_json(
        &self,
        branch_name: &str,
        symbol_or_name: &str,
    ) -> Result<String> {
        let payload = self.why_effect_branch_value(branch_name, symbol_or_name)?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn why_effect_branch(&self, branch_name: &str, symbol_or_name: &str) -> Result<String> {
        let payload = self.why_effect_branch_value(branch_name, symbol_or_name)?;
        Ok(format_v2_why(&payload))
    }

    fn why_effect_branch_value(
        &self,
        branch_name: &str,
        symbol_or_name: &str,
    ) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let symbol = self.resolve_symbol_for_blame(&branch.root_hash, symbol_or_name)?;
        let entry = self
            .root_symbol(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol is not in root: {symbol}"))?;
        let binding = self.preferred_binding(&root, &symbol);
        let declared_effects = self.signature_effect_names(&entry.signature)?;
        let is_external = self.definition_is_external(&entry.definition)?;
        let body_hash = if is_external {
            None
        } else {
            Some(self.function_body_hash(&entry.definition)?)
        };
        let mut required_by = BTreeMap::<String, Vec<JsonValue>>::new();
        if is_external {
            let external = self.external_function_metadata(&entry.definition)?;
            for effect in self.signature_effects(&entry.signature)? {
                required_by
                    .entry(effect.as_str().to_string())
                    .or_default()
                    .push(json!({
                        "kind": "external_function",
                        "abi": external.abi,
                        "link_name": external.link_name,
                        "library": external.library,
                        "definition_hash": entry.definition,
                    }));
            }
        }
        if let Some(body_hash) = &body_hash {
            if self.expr_requires_state(body_hash)? {
                required_by
                    .entry(Effect::State.as_str().to_string())
                    .or_default()
                    .push(json!({
                        "kind": "body_expression",
                        "reason": "assignment or mutable semantic place update",
                        "body_hash": body_hash,
                    }));
            }
            if self.expr_requires_alloc(body_hash)? {
                required_by
                    .entry(Effect::Alloc.as_str().to_string())
                    .or_default()
                    .push(json!({
                        "kind": "body_expression",
                        "reason": "heap/string/vector allocation expression",
                        "body_hash": body_hash,
                    }));
            }
            if self.expr_requires_unsafe(body_hash)? {
                required_by
                    .entry(Effect::Unsafe.as_str().to_string())
                    .or_default()
                    .push(json!({
                        "kind": "body_expression",
                        "reason": "raw pointer or unsafe expression",
                        "body_hash": body_hash,
                    }));
            }
        }
        for dependency in self.dependencies_for_definition(&root, &entry.definition)? {
            let Some(callee) = self.root_symbol(&root, &dependency) else {
                continue;
            };
            let callee_name = self
                .preferred_binding(&root, &dependency)
                .map(|binding| format!("{}.{}", binding.module, binding.display_name));
            for effect in self.signature_effects(&callee.signature)? {
                required_by
                    .entry(effect.as_str().to_string())
                    .or_default()
                    .push(json!({
                        "kind": "callee",
                        "symbol_hash": dependency,
                        "name": callee_name,
                        "signature_hash": callee.signature,
                    }));
            }
        }
        let missing_declarations = required_by
            .keys()
            .filter(|effect| !declared_effects.iter().any(|declared| declared == *effect))
            .cloned()
            .collect::<Vec<_>>();
        Ok(json!({
            "schema": WHY_EFFECT_SCHEMA,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "symbol_hash": symbol,
            "module": binding.map(|binding| binding.module.as_str()),
            "name": binding.map(|binding| binding.display_name.as_str()),
            "signature_hash": entry.signature,
            "definition_hash": entry.definition,
            "body_hash": body_hash,
            "declared_effects": declared_effects,
            "required_by": required_by,
            "missing_declarations": missing_declarations,
            "symbol_blame": self.blame_symbol_branch_value(branch_name, symbol_or_name)?,
        }))
    }

    pub fn why_platform_extern_branch_json(
        &mut self,
        branch_name: &str,
        entry_name: &str,
        extern_name: &str,
        target_triple: &str,
    ) -> Result<String> {
        let payload = self.why_platform_extern_branch_value(
            branch_name,
            entry_name,
            extern_name,
            target_triple,
        )?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn why_platform_extern_branch(
        &mut self,
        branch_name: &str,
        entry_name: &str,
        extern_name: &str,
        target_triple: &str,
    ) -> Result<String> {
        let payload = self.why_platform_extern_branch_value(
            branch_name,
            entry_name,
            extern_name,
            target_triple,
        )?;
        Ok(format_v2_why(&payload))
    }

    fn why_platform_extern_branch_value(
        &mut self,
        branch_name: &str,
        entry_name: &str,
        extern_name: &str,
        target_triple: &str,
    ) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let build_plan: JsonValue = serde_json::from_str(
            self.build_plan_branch(branch_name, entry_name, target_triple)?
                .trim_end(),
        )?;
        let matches = build_plan
            .get("platform_external_symbols")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter(|external| {
                external.get("link_name").and_then(JsonValue::as_str) == Some(extern_name)
                    || external.get("symbol_hash").and_then(JsonValue::as_str) == Some(extern_name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if matches.is_empty() {
            bail!("platform extern {extern_name:?} is not reachable from entry {entry_name:?}");
        }
        let semantic_externals = matches
            .iter()
            .filter_map(|external| external.get("symbol_hash").and_then(JsonValue::as_str))
            .filter_map(|symbol| {
                root.symbols
                    .iter()
                    .find(|entry| entry.symbol == symbol)
                    .map(|entry| (symbol, entry))
            })
            .map(|(symbol, entry)| {
                let metadata = self.external_function_metadata(&entry.definition)?;
                Ok(json!({
                    "symbol_hash": symbol,
                    "definition_hash": entry.definition,
                    "signature_hash": entry.signature,
                    "effects": self.signature_effect_names(&entry.signature)?,
                    "abi": metadata.abi,
                    "link_name": metadata.link_name,
                    "library": metadata.library,
                    "blame": self.blame_symbol_branch_value(branch_name, symbol)?,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({
            "schema": WHY_PLATFORM_EXTERN_SCHEMA,
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "entry_name": entry_name,
            "target_triple": target_triple,
            "query": extern_name,
            "platform_external_symbols": matches,
            "semantic_externals": semantic_externals,
            "build_plan": {
                "schema": build_plan.get("schema").cloned().unwrap_or(JsonValue::Null),
                "entry_symbol_hash": build_plan.get("entry_symbol_hash").cloned().unwrap_or(JsonValue::Null),
                "entry_effects": build_plan.get("entry_effects").cloned().unwrap_or(JsonValue::Null),
                "entry_point": build_plan.get("entry_point").cloned().unwrap_or(JsonValue::Null),
                "link_plan_input_hash": build_plan.get("link_plan_input_hash").cloned().unwrap_or(JsonValue::Null),
                "link_plan_cache_key": build_plan.get("link_plan_cache_key").cloned().unwrap_or(JsonValue::Null),
                "link_plan_hash": build_plan.get("link_plan_hash").cloned().unwrap_or(JsonValue::Null),
                "capabilities": build_plan.get("capabilities").cloned().unwrap_or(JsonValue::Null),
                "external_symbols": build_plan.get("external_symbols").cloned().unwrap_or(JsonValue::Null),
            },
        }))
    }

    pub fn why_borrow_branch_json(
        &mut self,
        branch_name: &str,
        symbol_or_name: &str,
        candidate_body: Option<&str>,
    ) -> Result<String> {
        let payload = self.why_candidate_body_branch_value(
            branch_name,
            symbol_or_name,
            candidate_body,
            CandidateWhyKind::Borrow,
        )?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn why_borrow_branch(
        &mut self,
        branch_name: &str,
        symbol_or_name: &str,
        candidate_body: Option<&str>,
    ) -> Result<String> {
        let payload = self.why_candidate_body_branch_value(
            branch_name,
            symbol_or_name,
            candidate_body,
            CandidateWhyKind::Borrow,
        )?;
        Ok(format_v2_why(&payload))
    }

    pub fn why_move_branch_json(
        &mut self,
        branch_name: &str,
        symbol_or_name: &str,
        candidate_body: Option<&str>,
    ) -> Result<String> {
        let payload = self.why_candidate_body_branch_value(
            branch_name,
            symbol_or_name,
            candidate_body,
            CandidateWhyKind::Move,
        )?;
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub fn why_move_branch(
        &mut self,
        branch_name: &str,
        symbol_or_name: &str,
        candidate_body: Option<&str>,
    ) -> Result<String> {
        let payload = self.why_candidate_body_branch_value(
            branch_name,
            symbol_or_name,
            candidate_body,
            CandidateWhyKind::Move,
        )?;
        Ok(format_v2_why(&payload))
    }

    fn why_candidate_body_branch_value(
        &mut self,
        branch_name: &str,
        symbol_or_name: &str,
        candidate_body: Option<&str>,
        kind: CandidateWhyKind,
    ) -> Result<JsonValue> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let symbol = self.resolve_symbol_for_blame(&branch.root_hash, symbol_or_name)?;
        let entry = self
            .root_symbol(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol is not in root: {symbol}"))?;
        let binding = self
            .preferred_binding(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol has no preferred binding {symbol}"))?
            .clone();
        let current_body_hash = if self.definition_is_external(&entry.definition)? {
            None
        } else {
            Some(self.function_body_hash(&entry.definition)?)
        };
        let candidate = match candidate_body {
            Some(source) => {
                let raw = parse_expr_source(source)?;
                let diagnostic = self.rollback_replace_body_diagnostic(
                    &branch.root_hash,
                    &binding.module,
                    &symbol,
                    &binding.display_name,
                    &raw,
                )?;
                json!({
                    "body_source": source,
                    "status": diagnostic.status,
                    "diagnostic": diagnostic.message,
                    "classifications": classify_candidate_diagnostic(kind, diagnostic.message.as_deref()),
                    "branch_unchanged": self.branch(branch_name)?.root_hash == branch.root_hash,
                })
            }
            None => json!({
                "status": "current_root_valid",
                "diagnostic": JsonValue::Null,
                "classifications": [],
                "body_hash": current_body_hash,
            }),
        };
        Ok(json!({
            "schema": kind.schema(),
            "branch": branch_name,
            "root_hash": branch.root_hash,
            "history_hash": branch.history_hash,
            "symbol_hash": symbol,
            "module": binding.module,
            "name": binding.display_name,
            "signature_hash": entry.signature,
            "definition_hash": entry.definition,
            "body_hash": current_body_hash,
            "candidate": candidate,
            "symbol_blame": self.blame_symbol_branch_value(branch_name, symbol_or_name)?,
        }))
    }

    fn rollback_replace_body_diagnostic(
        &mut self,
        input_root: &str,
        module: &str,
        symbol: &str,
        name: &str,
        body: &crate::expr::RawExpr,
    ) -> Result<CandidateDiagnostic> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.apply_replace_body(input_root, module, symbol, name, body);
        let rollback = self.conn.execute_batch("ROLLBACK");
        if let Err(rollback_err) = rollback {
            return Err(match result {
                Ok(_) => anyhow!("rollback failed after candidate body validation: {rollback_err}"),
                Err(err) => err.context(format!("rollback failed: {rollback_err}")),
            });
        }
        Ok(match result {
            Ok(_) => CandidateDiagnostic {
                status: "valid".to_string(),
                message: None,
            },
            Err(err) => CandidateDiagnostic {
                status: "invalid".to_string(),
                message: Some(format!("{err:#}")),
            },
        })
    }

    fn resolve_type_for_why(
        &mut self,
        root: &ProgramRootPayload,
        type_or_name: &str,
    ) -> Result<ResolvedWhyType> {
        let type_hash = if type_or_name.starts_with("sha256:") {
            match self.get_kind(type_or_name)?.as_str() {
                "Type" => type_or_name.to_string(),
                "SymbolBirth" => self.named_type_hash_for_symbol(root, type_or_name)?,
                other => bail!("object {type_or_name} is {other}, not Type or SymbolBirth"),
            }
        } else {
            self.resolve_type_in_root(MAIN_BRANCH, root, type_or_name)?
        };
        let type_symbol = match self.type_spec(&type_hash)? {
            TypeSpec::Named { type_symbol, .. } => Some(type_symbol),
            _ => None,
        };
        let type_def_hash = type_symbol
            .as_deref()
            .and_then(|symbol| self.root_type(root, symbol))
            .map(|entry| entry.type_def.clone());
        Ok(ResolvedWhyType {
            type_hash,
            type_symbol,
            type_def_hash,
        })
    }

    fn named_type_hash_for_symbol(
        &mut self,
        root: &ProgramRootPayload,
        type_symbol: &str,
    ) -> Result<String> {
        let entry = self
            .root_type(root, type_symbol)
            .ok_or_else(|| anyhow!("type symbol is not in root: {type_symbol}"))?;
        let definition = self.type_definition(&entry.type_def)?;
        let region_args = definition
            .region_params()
            .iter()
            .map(|param| param.region.clone())
            .collect::<Vec<_>>();
        let payload = type_payload_for_spec(&TypeSpec::Named {
            type_symbol: type_symbol.to_string(),
            region_args,
        })?;
        self.put_object("Type", &payload)
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
            Operation::CreateExternalFunction { module, name, .. } => {
                if self
                    .resolve_name(&item.output_root, module, name)
                    .is_ok_and(|created| created == symbol)
                {
                    reasons.insert("birth");
                    reasons.insert("signature");
                    reasons.insert("name");
                }
            }
            Operation::CreateType { .. }
            | Operation::RenameType { .. }
            | Operation::MoveType { .. }
            | Operation::AddField { .. }
            | Operation::RenameField { .. }
            | Operation::RemoveField { .. }
            | Operation::AddVariant { .. }
            | Operation::RenameVariant { .. }
            | Operation::RemoveVariant { .. } => {}
            Operation::RenameSymbol {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    reasons.insert("name");
                    reasons.insert("rename");
                }
            }
            Operation::MoveSymbol {
                symbol: changed, ..
            } => {
                if changed == symbol {
                    reasons.insert("name");
                    reasons.insert("move");
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
            Operation::AddParameter { .. } | Operation::ConvertParamToReference { .. } => {
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

    fn resolve_type_for_blame(&self, root_hash: &str, type_or_name: &str) -> Result<String> {
        if type_or_name.starts_with("sha256:") {
            let kind = self.get_kind(type_or_name)?;
            if kind != "SymbolBirth" {
                bail!("object {type_or_name} is {kind}, not SymbolBirth");
            }
            return Ok(type_or_name.to_string());
        }
        self.resolve_type_name(root_hash, MAIN_BRANCH, type_or_name)
    }

    fn resolve_member_for_blame(
        &self,
        root: &ProgramRootPayload,
        type_symbol: &str,
        member_or_name: &str,
        is_field: bool,
    ) -> Result<String> {
        if member_or_name.starts_with("sha256:") {
            let kind = self.get_kind(member_or_name)?;
            if kind != "SymbolBirth" {
                bail!("object {member_or_name} is {kind}, not SymbolBirth");
            }
            return Ok(member_or_name.to_string());
        }
        if is_field {
            self.field_symbol_by_name(root, type_symbol, member_or_name)
        } else {
            self.variant_symbol_by_name(root, type_symbol, member_or_name)
        }
    }

    fn classify_type_migration(
        &self,
        item: &ProvenanceHistoryItem,
        type_symbol: &str,
    ) -> Result<Vec<&'static str>> {
        let mut reasons = BTreeSet::new();
        match &item.operation {
            Operation::CreateType { module, name, .. } => {
                if self
                    .resolve_type_name(&item.output_root, module, name)
                    .is_ok_and(|created| created == type_symbol)
                {
                    reasons.insert("birth");
                    reasons.insert("definition");
                    reasons.insert("name");
                }
            }
            Operation::RenameType {
                type_symbol: changed,
                ..
            } => {
                if changed == type_symbol {
                    reasons.insert("name");
                    reasons.insert("rename");
                }
            }
            Operation::MoveType {
                type_symbol: changed,
                ..
            } => {
                if changed == type_symbol {
                    reasons.insert("name");
                    reasons.insert("move");
                }
            }
            Operation::AddField {
                type_symbol: changed,
                ..
            }
            | Operation::RemoveField {
                type_symbol: changed,
                ..
            }
            | Operation::RenameField {
                type_symbol: changed,
                ..
            }
            | Operation::AddVariant {
                type_symbol: changed,
                ..
            }
            | Operation::RemoveVariant {
                type_symbol: changed,
                ..
            }
            | Operation::RenameVariant {
                type_symbol: changed,
                ..
            } => {
                if changed == type_symbol {
                    reasons.insert("definition");
                    reasons.insert("member");
                }
            }
            Operation::MergeBranch { .. } => {
                if self.type_changed_between(&item.input_root, &item.output_root, type_symbol)? {
                    reasons.insert("merge");
                    reasons.insert("definition");
                }
            }
            _ => {}
        }
        Ok(reasons.into_iter().collect())
    }

    fn classify_member_migration(
        &self,
        item: &ProvenanceHistoryItem,
        type_symbol: &str,
        member_symbol: &str,
        is_field: bool,
    ) -> Result<Vec<&'static str>> {
        let mut reasons = BTreeSet::new();
        match &item.operation {
            Operation::CreateType { module, name, .. } => {
                // A member born with its type (not via a later add_field/variant)
                // is introduced by the create_type operation.
                if self
                    .resolve_type_name(&item.output_root, module, name)
                    .is_ok_and(|created| created == type_symbol)
                    && self.type_has_member(
                        &item.output_root,
                        type_symbol,
                        member_symbol,
                        is_field,
                    )?
                {
                    reasons.insert("birth");
                    reasons.insert("name");
                }
            }
            Operation::AddField {
                type_symbol: changed,
                field,
                ..
            } if is_field => {
                if changed == type_symbol
                    && self
                        .field_symbol_by_name(
                            &self.load_root(&item.output_root)?,
                            changed,
                            &field.name,
                        )
                        .is_ok_and(|symbol| symbol == member_symbol)
                {
                    reasons.insert("birth");
                    reasons.insert("name");
                }
            }
            Operation::RenameField {
                type_symbol: changed,
                field_symbol: changed_member,
                ..
            } if is_field => {
                if changed == type_symbol && changed_member == member_symbol {
                    reasons.insert("name");
                    reasons.insert("rename");
                }
            }
            Operation::RemoveField {
                type_symbol: changed,
                field_symbol: changed_member,
                ..
            } if is_field => {
                if changed == type_symbol && changed_member == member_symbol {
                    reasons.insert("remove");
                }
            }
            Operation::AddVariant {
                type_symbol: changed,
                variant,
                ..
            } if !is_field => {
                if changed == type_symbol
                    && self
                        .variant_symbol_by_name(
                            &self.load_root(&item.output_root)?,
                            changed,
                            &variant.name,
                        )
                        .is_ok_and(|symbol| symbol == member_symbol)
                {
                    reasons.insert("birth");
                    reasons.insert("name");
                }
            }
            Operation::RenameVariant {
                type_symbol: changed,
                variant_symbol: changed_member,
                ..
            } if !is_field => {
                if changed == type_symbol && changed_member == member_symbol {
                    reasons.insert("name");
                    reasons.insert("rename");
                }
            }
            Operation::RemoveVariant {
                type_symbol: changed,
                variant_symbol: changed_member,
                ..
            } if !is_field => {
                if changed == type_symbol && changed_member == member_symbol {
                    reasons.insert("remove");
                }
            }
            Operation::MergeBranch { .. } => {
                if self.type_changed_between(&item.input_root, &item.output_root, type_symbol)? {
                    reasons.insert("merge");
                }
            }
            _ => {}
        }
        Ok(reasons.into_iter().collect())
    }

    fn type_has_member(
        &self,
        root_hash: &str,
        type_symbol: &str,
        member_symbol: &str,
        is_field: bool,
    ) -> Result<bool> {
        let root = self.load_root(root_hash)?;
        let Some(entry) = self.root_type(&root, type_symbol) else {
            return Ok(false);
        };
        let definition = self.type_definition(&entry.type_def)?;
        let members = match (&definition, is_field) {
            (TypeDefinition::Record { fields, .. }, true) => fields,
            (TypeDefinition::Enum { variants, .. }, false) => variants,
            _ => return Ok(false),
        };
        Ok(members
            .iter()
            .any(|member| member.member_symbol == member_symbol))
    }

    fn type_changed_between(
        &self,
        old_root: &str,
        new_root: &str,
        type_symbol: &str,
    ) -> Result<bool> {
        let old = self.load_root(old_root)?;
        let new = self.load_root(new_root)?;
        let old_def = self
            .root_type(&old, type_symbol)
            .map(|entry| &entry.type_def);
        let new_def = self
            .root_type(&new, type_symbol)
            .map(|entry| &entry.type_def);
        if old_def != new_def {
            return Ok(true);
        }
        let old_name = self
            .preferred_type_binding(&old, type_symbol)
            .map(|binding| (&binding.module, &binding.display_name));
        let new_name = self
            .preferred_type_binding(&new, type_symbol)
            .map(|binding| (&binding.module, &binding.display_name));
        Ok(old_name != new_name)
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
                    "actual": test_value_from_value(&actual)?,
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

    fn migration_span_between_roots(
        &self,
        branch_name: &str,
        from_root: &str,
        to_root: &str,
    ) -> Result<MigrationRootSpan> {
        let states = self.history_root_states(branch_name)?;
        if from_root == to_root {
            let history_hash = states
                .iter()
                .rev()
                .find(|state| state.root_hash == from_root)
                .and_then(|state| state.history_hash.clone());
            return Ok(MigrationRootSpan {
                from_history_hash: history_hash.clone(),
                to_history_hash: history_hash,
                migrations: Vec::new(),
            });
        }

        let mut best_pair = None::<(usize, usize)>;
        for (from_idx, from_state) in states.iter().enumerate() {
            if from_state.root_hash != from_root {
                continue;
            }
            for (to_idx, to_state) in states.iter().enumerate().skip(from_idx + 1) {
                if to_state.root_hash != to_root {
                    continue;
                }
                let candidate_len = to_idx - from_idx;
                let replace = best_pair.is_none_or(|(best_from, best_to)| {
                    let best_len = best_to - best_from;
                    candidate_len < best_len || (candidate_len == best_len && from_idx > best_from)
                });
                if replace {
                    best_pair = Some((from_idx, to_idx));
                }
                break;
            }
        }

        let Some((from_idx, to_idx)) = best_pair else {
            return Ok(MigrationRootSpan {
                from_history_hash: states
                    .iter()
                    .find(|state| state.root_hash == from_root)
                    .and_then(|state| state.history_hash.clone()),
                to_history_hash: states
                    .iter()
                    .find(|state| state.root_hash == to_root)
                    .and_then(|state| state.history_hash.clone()),
                migrations: Vec::new(),
            });
        };

        let migrations = states[from_idx + 1..=to_idx]
            .iter()
            .map(|state| state.migration_from_parent.clone())
            .collect();
        Ok(MigrationRootSpan {
            from_history_hash: states[from_idx].history_hash.clone(),
            to_history_hash: states[to_idx].history_hash.clone(),
            migrations,
        })
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
struct MigrationRootSpan {
    from_history_hash: Option<String>,
    to_history_hash: Option<String>,
    migrations: Vec<JsonValue>,
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
    if evaluation.get("predicate_kind").and_then(JsonValue::as_str) == Some("semantic_test") {
        if evaluation
            .get("test_result")
            .and_then(|result| result.get("kind"))
            .and_then(JsonValue::as_str)
            == Some("test_not_found")
            && evaluation
                .get("expected_status")
                .and_then(JsonValue::as_str)
                != Some("error")
        {
            return false;
        }
        return evaluation
            .get("actual_status")
            .and_then(JsonValue::as_str)
            .is_some();
    }
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

#[derive(Debug, Clone)]
struct ResolvedWhyType {
    type_hash: String,
    type_symbol: Option<String>,
    type_def_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct CandidateDiagnostic {
    status: String,
    message: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum CandidateWhyKind {
    Borrow,
    Move,
}

impl CandidateWhyKind {
    fn schema(self) -> &'static str {
        match self {
            CandidateWhyKind::Borrow => WHY_BORROW_SCHEMA,
            CandidateWhyKind::Move => WHY_MOVE_SCHEMA,
        }
    }
}

fn format_v2_why(payload: &JsonValue) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} ok\n",
        payload
            .get("schema")
            .and_then(JsonValue::as_str)
            .unwrap_or("codedb/why-v2")
    ));
    out.push_str(&format!(
        "root {}\n",
        payload
            .get("root_hash")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
    ));
    out.push_str(&format!(
        "history {}\n",
        payload
            .get("history_hash")
            .and_then(JsonValue::as_str)
            .unwrap_or("none")
    ));
    for key in [
        "symbol_hash",
        "type_hash",
        "type_symbol",
        "type_def_hash",
        "signature_hash",
        "definition_hash",
        "body_hash",
    ] {
        if let Some(value) = payload.get(key).and_then(JsonValue::as_str) {
            out.push_str(&format!("{key} {value}\n"));
        }
    }
    if let Some(candidate) = payload.get("candidate") {
        out.push_str(&format!(
            "candidate_status {}\n",
            candidate
                .get("status")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
        ));
        if let Some(diagnostic) = candidate.get("diagnostic").and_then(JsonValue::as_str) {
            out.push_str(&format!("diagnostic {diagnostic}\n"));
        }
    }
    if let Some(field_layout) = payload.get("field_layout")
        && !field_layout.is_null()
    {
        out.push_str(&format!(
            "field_offset {}\n",
            field_layout
                .get("offset_bytes")
                .and_then(JsonValue::as_u64)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ));
    }
    if let Some(required_by) = payload.get("required_by").and_then(JsonValue::as_object) {
        for (effect, reasons) in required_by {
            out.push_str(&format!(
                "effect {effect} reasons {}\n",
                reasons.as_array().map(Vec::len).unwrap_or(0)
            ));
        }
    }
    if let Some(externals) = payload
        .get("platform_external_symbols")
        .and_then(JsonValue::as_array)
    {
        for external in externals {
            out.push_str(&format!(
                "platform_extern {} {}\n",
                external
                    .get("symbol_hash")
                    .and_then(JsonValue::as_str)
                    .unwrap_or(""),
                external
                    .get("link_name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
            ));
        }
    }
    out
}

fn layout_field_by_name_or_symbol(layout: &JsonValue, field: &str) -> Result<JsonValue> {
    let fields = layout
        .get("fields")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| anyhow!("layout has no record fields"))?;
    fields
        .iter()
        .find(|entry| {
            entry.get("name").and_then(JsonValue::as_str) == Some(field)
                || entry.get("field_symbol").and_then(JsonValue::as_str) == Some(field)
        })
        .cloned()
        .ok_or_else(|| anyhow!("field {field:?} not found in layout"))
}

fn layout_explanation(field: Option<&str>, field_layout: &Option<JsonValue>) -> JsonValue {
    match (field, field_layout) {
        (Some(field), Some(layout)) => json!({
            "kind": "field_offset",
            "field": field,
            "offset_bytes": layout.get("offset_bytes").cloned().unwrap_or(JsonValue::Null),
            "rule": "record fields are laid out in semantic field order; each field offset is the previous size rounded up to the field alignment for the target",
        }),
        _ => json!({
            "kind": "type_layout",
            "rule": "type layout is recomputed from the semantic type hash, target triple, layout version, ABI version, and referenced type definitions",
        }),
    }
}

fn drop_reasons(layout: &JsonValue) -> Vec<JsonValue> {
    let mut reasons = Vec::new();
    if layout.get("copy_kind").and_then(JsonValue::as_str) == Some("move_only") {
        reasons.push(json!({
            "kind": "move_only",
            "rule": "values classified move_only cannot be implicitly copied; moves transfer ownership or carried loans",
        }));
    }
    if layout.get("drop_kind").and_then(JsonValue::as_str) == Some("needs_drop") {
        reasons.push(json!({
            "kind": "needs_drop",
            "rule": "layout contains an owned resource whose drop path may release native storage",
        }));
    }
    for (key, rule) in [
        (
            "contains_box",
            "box<T> owns heap storage and makes its containing type move-only",
        ),
        (
            "contains_owned_resource",
            "owned native resources require generated drop/free handling",
        ),
        (
            "contains_mut_reference",
            "mutable references carry exclusive loans and make their containing type move-only",
        ),
        (
            "contains_reference",
            "reference-containing values carry semantic loans through copies or moves",
        ),
    ] {
        if layout.get(key).and_then(JsonValue::as_bool) == Some(true) {
            reasons.push(json!({
                "kind": key,
                "rule": rule,
            }));
        }
    }
    if reasons.is_empty() {
        reasons.push(json!({
            "kind": "trivial",
            "rule": "layout is Copy and has trivial drop classification",
        }));
    }
    reasons
}

fn classify_candidate_diagnostic(
    kind: CandidateWhyKind,
    message: Option<&str>,
) -> Vec<&'static str> {
    let Some(message) = message else {
        return Vec::new();
    };
    let mut classifications = BTreeSet::new();
    match kind {
        CandidateWhyKind::Borrow => {
            if message.contains("exclusive loan conflict") {
                classifications.insert("exclusive_loan_conflict");
            }
            if message.contains("shared read") {
                classifications.insert("shared_read_conflict");
            }
            if message.contains("returns reference to local storage") {
                classifications.insert("local_borrow_escape");
            }
            if message.contains("bad_borrow") {
                classifications.insert("borrow_rule_violation");
            }
        }
        CandidateWhyKind::Move => {
            if message.contains("bad_move") || message.contains("use after move") {
                classifications.insert("use_after_move");
            }
            if message.contains("move of") && message.contains("conflicts with live") {
                classifications.insert("move_conflicts_with_live_borrow");
            }
            if message.contains("unsupported_move") {
                classifications.insert("unsupported_move_shape");
            }
        }
    }
    classifications.into_iter().collect()
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
        "fold" => {
            for key in ["target", "init", "body"] {
                children.push(
                    payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("fold missing {key}"))?
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
