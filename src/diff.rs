use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Value as JsonValue, json};

use crate::abi::internal_abi_symbol;
use crate::model::{ProgramRootPayload, preferred_type_binding};
use crate::store::{CodeDb, canonical_json};
use crate::types::{TypeDefinition, TypeMemberDef};

impl CodeDb {
    pub fn diff_roots(&self, root_a: &str, root_b: &str) -> Result<String> {
        let a = self.load_root(root_a)?;
        let b = self.load_root(root_b)?;
        let build_impact = self.plan_build_impact(root_a, root_b)?;
        let mut out = String::new();
        out.push_str("Root changed:\n");
        out.push_str(&format!("  from {root_a}\n"));
        out.push_str(&format!("  to   {root_b}\n\n"));
        if root_a == root_b {
            out.push_str("unchanged\n");
            return Ok(out);
        }

        let a_symbols = a
            .symbols
            .iter()
            .map(|entry| (entry.symbol.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let b_symbols = b
            .symbols
            .iter()
            .map(|entry| (entry.symbol.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let all_symbols = a_symbols
            .keys()
            .chain(b_symbols.keys())
            .cloned()
            .collect::<BTreeSet<_>>();

        let mut emitted = false;
        for symbol in all_symbols {
            match (a_symbols.get(&symbol), b_symbols.get(&symbol)) {
                (None, Some(_)) => {
                    emitted = true;
                    out.push_str("symbol_added:\n");
                    out.push_str(&format!(
                        "  symbol: {symbol}\n  name: {}\n\n",
                        self.symbol_display(&b, &symbol)?
                    ));
                }
                (Some(_), None) => {
                    emitted = true;
                    out.push_str("symbol_removed:\n");
                    out.push_str(&format!(
                        "  symbol: {symbol}\n  name: {}\n\n",
                        self.symbol_display(&a, &symbol)?
                    ));
                }
                (Some(a_entry), Some(b_entry)) => {
                    let a_binding = self
                        .preferred_binding(&a, &symbol)
                        .ok_or_else(|| anyhow::anyhow!("symbol has no name {symbol}"))?;
                    let b_binding = self
                        .preferred_binding(&b, &symbol)
                        .ok_or_else(|| anyhow::anyhow!("symbol has no name {symbol}"))?;
                    let a_name = format!("{}.{}", a_binding.module, a_binding.display_name);
                    let b_name = format!("{}.{}", b_binding.module, b_binding.display_name);
                    if a_name != b_name {
                        emitted = true;
                        if a_binding.display_name == b_binding.display_name {
                            out.push_str("symbol_moved:\n");
                        } else {
                            out.push_str("symbol_renamed:\n");
                        }
                        out.push_str(&format!("  symbol: {symbol}\n  {a_name} -> {b_name}\n"));
                        if a_entry.signature == b_entry.signature {
                            out.push_str("  signature hash: unchanged\n");
                        }
                        if self.definition_body_hash_opt(&a_entry.definition)?
                            == self.definition_body_hash_opt(&b_entry.definition)?
                        {
                            out.push_str("  function body hash: unchanged\n");
                        }
                        out.push_str("  compile impact: metadata_only\n\n");
                    }

                    let a_aliases = qualified_aliases_for(&a, &symbol);
                    let b_aliases = qualified_aliases_for(&b, &symbol);
                    for alias in b_aliases.difference(&a_aliases) {
                        emitted = true;
                        out.push_str("alias_added:\n");
                        out.push_str(&format!("  symbol: {symbol}\n  alias: {alias}\n\n"));
                    }
                    for alias in a_aliases.difference(&b_aliases) {
                        emitted = true;
                        out.push_str("alias_removed:\n");
                        out.push_str(&format!("  symbol: {symbol}\n  alias: {alias}\n\n"));
                    }

                    if a_entry.signature != b_entry.signature {
                        emitted = true;
                        out.push_str("interface_changed:\n");
                        out.push_str(&format!(
                            "  function: {b_name}\n  symbol: {symbol}\n  from: {}\n  to:   {}\n  compile impact: recompile_dependents\n\n",
                            a_entry.signature, b_entry.signature
                        ));
                    } else if a_entry.definition != b_entry.definition {
                        emitted = true;
                        out.push_str("implementation_changed:\n");
                        out.push_str(&format!(
                            "  function: {b_name}\n  symbol: {symbol}\n  signature: unchanged\n  compile impact: recompile_symbols\n"
                        ));
                        if let (Some(a_body), Some(b_body)) = (
                            self.definition_body_hash_opt(&a_entry.definition)?,
                            self.definition_body_hash_opt(&b_entry.definition)?,
                        ) {
                            self.diff_exprs(&a, &b, &a_body, &b_body, &mut out, "  ")?;
                        }
                        out.push('\n');
                    }
                }
                (None, None) => unreachable!(),
            }
        }

        let deps_a = dependency_pairs(&self.conn, root_a)?;
        let deps_b = dependency_pairs(&self.conn, root_b)?;
        for dep in deps_b.difference(&deps_a) {
            emitted = true;
            out.push_str("dependency_added:\n");
            out.push_str(&format!("  {} -> {}\n\n", dep.0, dep.1));
        }
        for dep in deps_a.difference(&deps_b) {
            emitted = true;
            out.push_str("dependency_removed:\n");
            out.push_str(&format!("  {} -> {}\n\n", dep.0, dep.1));
        }

        let exports_a = export_pairs(&a);
        let exports_b = export_pairs(&b);
        for export in exports_b.difference(&exports_a) {
            emitted = true;
            out.push_str("export_added:\n");
            out.push_str(&format!(
                "  symbol: {}\n  internal_abi_symbol: {}\n  exported_abi_symbol: {}\n  compile impact: relink_only\n\n",
                export.0,
                internal_abi_symbol(&export.0)?,
                export.1
            ));
        }
        for export in exports_a.difference(&exports_b) {
            emitted = true;
            out.push_str("export_removed:\n");
            out.push_str(&format!(
                "  symbol: {}\n  internal_abi_symbol: {}\n  exported_abi_symbol: {}\n  compile impact: relink_only\n\n",
                export.0,
                internal_abi_symbol(&export.0)?,
                export.1
            ));
        }

        for record in self.type_diff_records(&a, &b)? {
            emitted = true;
            render_type_change_text(&record, &mut out);
        }

        if !emitted {
            out.push_str("Only root metadata or ordering changed.\n");
        }
        if root_a != root_b {
            out.push_str("Incremental build impact:\n");
            build_impact.push_cli_lines(&mut out);
        }
        Ok(out)
    }

    pub fn diff_roots_json(&self, root_a: &str, root_b: &str) -> Result<String> {
        let changes = self.diff_change_json(root_a, root_b)?;
        let build_impact = self.plan_build_impact(root_a, root_b)?;
        let payload = json!({
            "old_root_hash": root_a,
            "new_root_hash": root_b,
            "changes": changes,
            "build_impact": build_impact.to_json(),
        });
        Ok(format!("{}\n", canonical_json(&payload)))
    }

    pub(crate) fn diff_change_json(&self, root_a: &str, root_b: &str) -> Result<Vec<JsonValue>> {
        let a = self.load_root(root_a)?;
        let b = self.load_root(root_b)?;
        let a_symbols = a
            .symbols
            .iter()
            .map(|entry| (entry.symbol.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let b_symbols = b
            .symbols
            .iter()
            .map(|entry| (entry.symbol.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let all_symbols = a_symbols
            .keys()
            .chain(b_symbols.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut changes = Vec::new();

        for symbol in all_symbols {
            // Derived symbols — a generic function's monomorphic instances (R11)
            // — have no name binding and are not part of the semantic surface;
            // they are reproduced deterministically on re-import, so a change to
            // a generic shows up as a change to the generic, not its instances.
            if self.preferred_binding(&a, &symbol).is_none()
                && self.preferred_binding(&b, &symbol).is_none()
            {
                continue;
            }
            match (a_symbols.get(&symbol), b_symbols.get(&symbol)) {
                (None, Some(_)) => changes.push(json!({
                    "kind": "symbol_added",
                    "symbol": &symbol,
                    "name": self.symbol_display(&b, &symbol)?,
                })),
                (Some(_), None) => changes.push(json!({
                    "kind": "symbol_removed",
                    "symbol": &symbol,
                    "name": self.symbol_display(&a, &symbol)?,
                })),
                (Some(a_entry), Some(b_entry)) => {
                    let a_binding = self
                        .preferred_binding(&a, &symbol)
                        .ok_or_else(|| anyhow::anyhow!("symbol has no name {symbol}"))?;
                    let b_binding = self
                        .preferred_binding(&b, &symbol)
                        .ok_or_else(|| anyhow::anyhow!("symbol has no name {symbol}"))?;
                    let a_name = format!("{}.{}", a_binding.module, a_binding.display_name);
                    let b_name = format!("{}.{}", b_binding.module, b_binding.display_name);
                    if a_name != b_name {
                        changes.push(json!({
                            "kind": if a_binding.display_name == b_binding.display_name { "symbol_moved" } else { "symbol_renamed" },
                            "symbol": &symbol,
                            "from": a_name,
                            "to": b_name,
                            "signature_hash_unchanged": a_entry.signature == b_entry.signature,
                            "body_hash_unchanged": self.definition_body_hash_opt(&a_entry.definition)?
                                == self.definition_body_hash_opt(&b_entry.definition)?,
                        }));
                    }

                    let a_aliases = qualified_aliases_for(&a, &symbol);
                    let b_aliases = qualified_aliases_for(&b, &symbol);
                    for alias in b_aliases.difference(&a_aliases) {
                        changes.push(json!({
                            "kind": "alias_added",
                            "symbol": &symbol,
                            "alias": alias,
                        }));
                    }
                    for alias in a_aliases.difference(&b_aliases) {
                        changes.push(json!({
                            "kind": "alias_removed",
                            "symbol": &symbol,
                            "alias": alias,
                        }));
                    }

                    if a_entry.signature != b_entry.signature {
                        changes.push(json!({
                            "kind": "interface_changed",
                            "symbol": &symbol,
                            "function": b_name,
                            "from": &a_entry.signature,
                            "to": &b_entry.signature,
                        }));
                    } else if a_entry.definition != b_entry.definition {
                        let from_body = self.definition_body_hash_opt(&a_entry.definition)?;
                        let to_body = self.definition_body_hash_opt(&b_entry.definition)?;
                        // Hash-pruned tree diff: descend into the body only where
                        // content hashes differ. Identical subtrees are skipped
                        // (and never loaded) because equal hash => equal subtree.
                        let mut expr_changes = Vec::new();
                        if let (Some(from), Some(to)) = (&from_body, &to_body) {
                            self.diff_exprs_json(&a, &b, from, to, "body", &mut expr_changes)?;
                        }
                        changes.push(json!({
                            "kind": "implementation_changed",
                            "symbol": &symbol,
                            "function": b_name,
                            "signature_hash_unchanged": true,
                            "from_body": from_body,
                            "to_body": to_body,
                            "expr_changes": expr_changes,
                        }));
                    }
                }
                (None, None) => unreachable!(),
            }
        }

        let deps_a = dependency_pairs(&self.conn, root_a)?;
        let deps_b = dependency_pairs(&self.conn, root_b)?;
        for dep in deps_b.difference(&deps_a) {
            changes.push(json!({
                "kind": "dependency_added",
                "from": &dep.0,
                "to": &dep.1,
            }));
        }
        for dep in deps_a.difference(&deps_b) {
            changes.push(json!({
                "kind": "dependency_removed",
                "from": &dep.0,
                "to": &dep.1,
            }));
        }
        let exports_a = export_pairs(&a);
        let exports_b = export_pairs(&b);
        for export in exports_b.difference(&exports_a) {
            changes.push(json!({
                "kind": "export_added",
                "symbol": &export.0,
                "internal_abi_symbol": internal_abi_symbol(&export.0)?,
                "exported_abi_symbol": &export.1,
            }));
        }
        for export in exports_a.difference(&exports_b) {
            changes.push(json!({
                "kind": "export_removed",
                "symbol": &export.0,
                "internal_abi_symbol": internal_abi_symbol(&export.0)?,
                "exported_abi_symbol": &export.1,
            }));
        }
        changes.extend(self.type_diff_records(&a, &b)?);
        Ok(changes)
    }

    /// Structured change records for type definitions (added/removed/renamed/
    /// moved types and per-member field/variant changes keyed by stable member
    /// identity). Shared by the text and JSON diff paths.
    fn type_diff_records(
        &self,
        a: &ProgramRootPayload,
        b: &ProgramRootPayload,
    ) -> Result<Vec<JsonValue>> {
        let a_types = a
            .types
            .iter()
            .map(|entry| (entry.type_symbol.as_str(), entry.type_def.as_str()))
            .collect::<BTreeMap<_, _>>();
        let b_types = b
            .types
            .iter()
            .map(|entry| (entry.type_symbol.as_str(), entry.type_def.as_str()))
            .collect::<BTreeMap<_, _>>();
        let all_types = a_types
            .keys()
            .chain(b_types.keys())
            .copied()
            .collect::<BTreeSet<_>>();

        let mut records = Vec::new();
        for type_symbol in all_types {
            match (a_types.get(type_symbol), b_types.get(type_symbol)) {
                (None, Some(_)) => records.push(json!({
                    "kind": "type_added",
                    "type_symbol": type_symbol,
                    "name": type_display(b, type_symbol),
                })),
                (Some(_), None) => records.push(json!({
                    "kind": "type_removed",
                    "type_symbol": type_symbol,
                    "name": type_display(a, type_symbol),
                })),
                (Some(a_def), Some(b_def)) => {
                    let a_name = type_display(a, type_symbol);
                    let b_name = type_display(b, type_symbol);
                    if a_name != b_name {
                        let moved = preferred_type_binding(a, type_symbol)
                            .map(|binding| binding.display_name.as_str())
                            == preferred_type_binding(b, type_symbol)
                                .map(|binding| binding.display_name.as_str());
                        records.push(json!({
                            "kind": if moved { "type_moved" } else { "type_renamed" },
                            "type_symbol": type_symbol,
                            "from": a_name,
                            "to": b_name,
                        }));
                    }
                    if a_def != b_def {
                        self.push_member_diff_records(
                            a_def,
                            b_def,
                            type_symbol,
                            &b_name,
                            &mut records,
                        )?;
                    }
                }
                (None, None) => unreachable!(),
            }
        }
        Ok(records)
    }

    fn push_member_diff_records(
        &self,
        a_def_hash: &str,
        b_def_hash: &str,
        type_symbol: &str,
        type_name: &str,
        records: &mut Vec<JsonValue>,
    ) -> Result<()> {
        let a_def = self.type_definition(a_def_hash)?;
        let b_def = self.type_definition(b_def_hash)?;
        if a_def.kind_name() != b_def.kind_name() {
            records.push(json!({
                "kind": "type_definition_changed",
                "type_symbol": type_symbol,
                "name": type_name,
                "from_kind": a_def.kind_name(),
                "to_kind": b_def.kind_name(),
            }));
            return Ok(());
        }
        let label = member_label(&b_def);
        let a_members = members_of(&a_def)
            .iter()
            .map(|member| (member.member_symbol.as_str(), member))
            .collect::<BTreeMap<_, _>>();
        let b_members = members_of(&b_def)
            .iter()
            .map(|member| (member.member_symbol.as_str(), member))
            .collect::<BTreeMap<_, _>>();
        let all_members = a_members
            .keys()
            .chain(b_members.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        let mut member_change = false;
        for member_symbol in all_members {
            match (a_members.get(member_symbol), b_members.get(member_symbol)) {
                (None, Some(member)) => {
                    member_change = true;
                    records.push(json!({
                        "kind": format!("{label}_added"),
                        "type_symbol": type_symbol,
                        "type_name": type_name,
                        "member_symbol": member_symbol,
                        "member_name": member.name,
                    }));
                }
                (Some(member), None) => {
                    member_change = true;
                    records.push(json!({
                        "kind": format!("{label}_removed"),
                        "type_symbol": type_symbol,
                        "type_name": type_name,
                        "member_symbol": member_symbol,
                        "member_name": member.name,
                    }));
                }
                (Some(a_member), Some(b_member)) => {
                    if a_member.name != b_member.name {
                        member_change = true;
                        records.push(json!({
                            "kind": format!("{label}_renamed"),
                            "type_symbol": type_symbol,
                            "type_name": type_name,
                            "member_symbol": member_symbol,
                            "from": a_member.name,
                            "to": b_member.name,
                        }));
                    }
                    if a_member.type_hash != b_member.type_hash {
                        member_change = true;
                        records.push(json!({
                            "kind": format!("{label}_type_changed"),
                            "type_symbol": type_symbol,
                            "type_name": type_name,
                            "member_symbol": member_symbol,
                            "member_name": b_member.name,
                        }));
                    }
                }
                (None, None) => unreachable!(),
            }
        }
        // A definition-hash change with no member identity change (e.g. region
        // parameters changed) must still be reported, never silently dropped.
        if !member_change {
            records.push(json!({
                "kind": "type_definition_changed",
                "type_symbol": type_symbol,
                "name": type_name,
            }));
        }
        Ok(())
    }

    fn diff_exprs(
        &self,
        root_a: &ProgramRootPayload,
        root_b: &ProgramRootPayload,
        expr_a: &str,
        expr_b: &str,
        out: &mut String,
        indent: &str,
    ) -> Result<()> {
        if expr_a == expr_b {
            out.push_str(&format!("{indent}expression unchanged by hash {expr_a}\n"));
            return Ok(());
        }
        let a = self.get_payload(expr_a)?;
        let b = self.get_payload(expr_b)?;
        let kind_a = a
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .unwrap_or("?");
        let kind_b = b
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .unwrap_or("?");
        if kind_a != kind_b {
            out.push_str(&format!(
                "{indent}expression_replaced: {kind_a} -> {kind_b}\n"
            ));
            return Ok(());
        }
        match kind_a {
            "literal_i64" | "literal_bool" => {
                out.push_str(&format!(
                    "{indent}literal_changed: {} -> {}\n",
                    short_json(a.get("value").unwrap_or(&JsonValue::Null)),
                    short_json(b.get("value").unwrap_or(&JsonValue::Null))
                ));
            }
            "literal_unit" => {
                out.push_str(&format!("{indent}unit_literal_changed_by_hash\n"));
            }
            "call" => {
                let sym_a = a.get("symbol").and_then(JsonValue::as_str).unwrap_or("");
                let sym_b = b.get("symbol").and_then(JsonValue::as_str).unwrap_or("");
                if sym_a != sym_b {
                    out.push_str(&format!(
                        "{indent}call_target_changed: {} -> {}\n",
                        self.symbol_display(root_a, sym_a)
                            .unwrap_or_else(|_| sym_a.to_string()),
                        self.symbol_display(root_b, sym_b)
                            .unwrap_or_else(|_| sym_b.to_string())
                    ));
                }
                let args_a = a
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                let args_b = b
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (idx, (arg_a, arg_b)) in args_a.iter().zip(args_b.iter()).enumerate() {
                    out.push_str(&format!("{indent}arg {idx}:\n"));
                    self.diff_exprs(
                        root_a,
                        root_b,
                        arg_a.as_str().unwrap_or(""),
                        arg_b.as_str().unwrap_or(""),
                        out,
                        &format!("{indent}  "),
                    )?;
                }
            }
            "binary" => {
                let op_a = a.get("op").and_then(JsonValue::as_str).unwrap_or("");
                let op_b = b.get("op").and_then(JsonValue::as_str).unwrap_or("");
                if op_a != op_b {
                    out.push_str(&format!(
                        "{indent}expression_replaced: op {op_a} -> {op_b}\n"
                    ));
                }
                for key in ["left", "right"] {
                    out.push_str(&format!("{indent}{key}:\n"));
                    self.diff_exprs(
                        root_a,
                        root_b,
                        a.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        b.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        out,
                        &format!("{indent}  "),
                    )?;
                }
            }
            "unary" => {
                let op_a = a.get("op").and_then(JsonValue::as_str).unwrap_or("");
                let op_b = b.get("op").and_then(JsonValue::as_str).unwrap_or("");
                if op_a != op_b {
                    out.push_str(&format!(
                        "{indent}expression_replaced: unary op {op_a} -> {op_b}\n"
                    ));
                }
                out.push_str(&format!("{indent}expr:\n"));
                self.diff_exprs(
                    root_a,
                    root_b,
                    a.get("expr").and_then(JsonValue::as_str).unwrap_or(""),
                    b.get("expr").and_then(JsonValue::as_str).unwrap_or(""),
                    out,
                    &format!("{indent}  "),
                )?;
            }
            "let" => {
                if a.get("binding_type") != b.get("binding_type") {
                    out.push_str(&format!(
                        "{indent}let_binding_type_changed: {} -> {}\n",
                        short_json(a.get("binding_type").unwrap_or(&JsonValue::Null)),
                        short_json(b.get("binding_type").unwrap_or(&JsonValue::Null))
                    ));
                }
                if a.get("binding_name") != b.get("binding_name") {
                    out.push_str(&format!(
                        "{indent}let_binding_name_changed: {} -> {}\n",
                        short_json(a.get("binding_name").unwrap_or(&JsonValue::Null)),
                        short_json(b.get("binding_name").unwrap_or(&JsonValue::Null))
                    ));
                }
                for key in ["value", "body"] {
                    out.push_str(&format!("{indent}{key}:\n"));
                    self.diff_exprs(
                        root_a,
                        root_b,
                        a.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        b.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        out,
                        &format!("{indent}  "),
                    )?;
                }
            }
            "if" => {
                for key in ["cond", "then", "else"] {
                    out.push_str(&format!("{indent}{key}:\n"));
                    self.diff_exprs(
                        root_a,
                        root_b,
                        a.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        b.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        out,
                        &format!("{indent}  "),
                    )?;
                }
            }
            "return" => {
                out.push_str(&format!("{indent}value:\n"));
                self.diff_exprs(
                    root_a,
                    root_b,
                    a.get("value").and_then(JsonValue::as_str).unwrap_or(""),
                    b.get("value").and_then(JsonValue::as_str).unwrap_or(""),
                    out,
                    &format!("{indent}  "),
                )?;
            }
            "param_ref" => {
                if a.get("index") != b.get("index") {
                    out.push_str(&format!(
                        "{indent}expression_replaced: param_ref {} -> {}\n",
                        short_json(a.get("index").unwrap_or(&JsonValue::Null)),
                        short_json(b.get("index").unwrap_or(&JsonValue::Null))
                    ));
                }
            }
            "local_ref" => {
                if a.get("depth") != b.get("depth") {
                    out.push_str(&format!(
                        "{indent}expression_replaced: local_ref {} -> {}\n",
                        short_json(a.get("depth").unwrap_or(&JsonValue::Null)),
                        short_json(b.get("depth").unwrap_or(&JsonValue::Null))
                    ));
                }
            }
            _ => out.push_str(&format!("{indent}expression_replaced\n")),
        }
        let type_a = a.get("type").and_then(JsonValue::as_str).unwrap_or("");
        let type_b = b.get("type").and_then(JsonValue::as_str).unwrap_or("");
        if type_a != type_b {
            out.push_str(&format!("{indent}type_changed: {type_a} -> {type_b}\n"));
        }
        Ok(())
    }

    /// Hash-pruned JSON tree diff of two expression subtrees. Structured sibling
    /// of `diff_exprs`, emitting records into `changes` with a `path` locating
    /// each change within the body.
    ///
    /// The prune is exact, not heuristic: every node hash is a Merkle hash over
    /// the node's payload including all transitive child hashes, so `expr_a ==
    /// expr_b` means the two subtrees are byte-identical — skipped without
    /// loading either payload. Conversely, any descendant difference forces every
    /// ancestor hash (up to the body and the symbol's definition hash) to differ,
    /// so the walk always reaches a real change before pruning. Unrecognized
    /// expression kinds report a single `expression_replaced` at their path
    /// rather than descending — conservative, and never dropping a change.
    fn diff_exprs_json(
        &self,
        root_a: &ProgramRootPayload,
        root_b: &ProgramRootPayload,
        expr_a: &str,
        expr_b: &str,
        path: &str,
        changes: &mut Vec<JsonValue>,
    ) -> Result<()> {
        if expr_a == expr_b {
            return Ok(());
        }
        let a = self.get_payload(expr_a)?;
        let b = self.get_payload(expr_b)?;
        let kind_a = a.get("expr_kind").and_then(JsonValue::as_str).unwrap_or("?");
        let kind_b = b.get("expr_kind").and_then(JsonValue::as_str).unwrap_or("?");
        if kind_a != kind_b {
            changes.push(json!({
                "kind": "expression_replaced",
                "path": path,
                "from": kind_a,
                "to": kind_b,
            }));
            return Ok(());
        }
        match kind_a {
            "literal_i64" | "literal_bool" => {
                changes.push(json!({
                    "kind": "literal_changed",
                    "path": path,
                    "from": a.get("value").cloned().unwrap_or(JsonValue::Null),
                    "to": b.get("value").cloned().unwrap_or(JsonValue::Null),
                }));
            }
            "literal_unit" => {
                changes.push(json!({
                    "kind": "expression_replaced",
                    "path": path,
                    "from": "literal_unit",
                    "to": "literal_unit",
                }));
            }
            "call" => {
                let sym_a = a.get("symbol").and_then(JsonValue::as_str).unwrap_or("");
                let sym_b = b.get("symbol").and_then(JsonValue::as_str).unwrap_or("");
                if sym_a != sym_b {
                    changes.push(json!({
                        "kind": "call_target_changed",
                        "path": path,
                        "from": self
                            .symbol_display(root_a, sym_a)
                            .unwrap_or_else(|_| sym_a.to_string()),
                        "to": self
                            .symbol_display(root_b, sym_b)
                            .unwrap_or_else(|_| sym_b.to_string()),
                    }));
                }
                let args_a = a
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                let args_b = b
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (idx, (arg_a, arg_b)) in args_a.iter().zip(args_b.iter()).enumerate() {
                    self.diff_exprs_json(
                        root_a,
                        root_b,
                        arg_a.as_str().unwrap_or(""),
                        arg_b.as_str().unwrap_or(""),
                        &child_path(path, &format!("arg{idx}")),
                        changes,
                    )?;
                }
            }
            "binary" => {
                let op_a = a.get("op").and_then(JsonValue::as_str).unwrap_or("");
                let op_b = b.get("op").and_then(JsonValue::as_str).unwrap_or("");
                if op_a != op_b {
                    changes.push(json!({
                        "kind": "expression_replaced",
                        "path": path,
                        "from": format!("binary {op_a}"),
                        "to": format!("binary {op_b}"),
                    }));
                }
                for key in ["left", "right"] {
                    self.diff_exprs_json(
                        root_a,
                        root_b,
                        a.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        b.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        &child_path(path, key),
                        changes,
                    )?;
                }
            }
            "unary" => {
                let op_a = a.get("op").and_then(JsonValue::as_str).unwrap_or("");
                let op_b = b.get("op").and_then(JsonValue::as_str).unwrap_or("");
                if op_a != op_b {
                    changes.push(json!({
                        "kind": "expression_replaced",
                        "path": path,
                        "from": format!("unary {op_a}"),
                        "to": format!("unary {op_b}"),
                    }));
                }
                self.diff_exprs_json(
                    root_a,
                    root_b,
                    a.get("expr").and_then(JsonValue::as_str).unwrap_or(""),
                    b.get("expr").and_then(JsonValue::as_str).unwrap_or(""),
                    &child_path(path, "expr"),
                    changes,
                )?;
            }
            "let" => {
                if a.get("binding_type") != b.get("binding_type") {
                    changes.push(json!({
                        "kind": "let_binding_type_changed",
                        "path": path,
                        "from": a.get("binding_type").cloned().unwrap_or(JsonValue::Null),
                        "to": b.get("binding_type").cloned().unwrap_or(JsonValue::Null),
                    }));
                }
                if a.get("binding_name") != b.get("binding_name") {
                    changes.push(json!({
                        "kind": "let_binding_name_changed",
                        "path": path,
                        "from": a.get("binding_name").cloned().unwrap_or(JsonValue::Null),
                        "to": b.get("binding_name").cloned().unwrap_or(JsonValue::Null),
                    }));
                }
                for key in ["value", "body"] {
                    self.diff_exprs_json(
                        root_a,
                        root_b,
                        a.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        b.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        &child_path(path, key),
                        changes,
                    )?;
                }
            }
            "if" => {
                for key in ["cond", "then", "else"] {
                    self.diff_exprs_json(
                        root_a,
                        root_b,
                        a.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        b.get(key).and_then(JsonValue::as_str).unwrap_or(""),
                        &child_path(path, key),
                        changes,
                    )?;
                }
            }
            "return" => {
                self.diff_exprs_json(
                    root_a,
                    root_b,
                    a.get("value").and_then(JsonValue::as_str).unwrap_or(""),
                    b.get("value").and_then(JsonValue::as_str).unwrap_or(""),
                    &child_path(path, "value"),
                    changes,
                )?;
            }
            "param_ref" => {
                if a.get("index") != b.get("index") {
                    changes.push(json!({
                        "kind": "expression_replaced",
                        "path": path,
                        "from": a.get("index").cloned().unwrap_or(JsonValue::Null),
                        "to": b.get("index").cloned().unwrap_or(JsonValue::Null),
                    }));
                }
            }
            "local_ref" => {
                if a.get("depth") != b.get("depth") {
                    changes.push(json!({
                        "kind": "expression_replaced",
                        "path": path,
                        "from": a.get("depth").cloned().unwrap_or(JsonValue::Null),
                        "to": b.get("depth").cloned().unwrap_or(JsonValue::Null),
                    }));
                }
            }
            _ => changes.push(json!({
                "kind": "expression_replaced",
                "path": path,
            })),
        }
        let type_a = a.get("type").and_then(JsonValue::as_str).unwrap_or("");
        let type_b = b.get("type").and_then(JsonValue::as_str).unwrap_or("");
        if type_a != type_b {
            changes.push(json!({
                "kind": "type_changed",
                "path": path,
                "from": type_a,
                "to": type_b,
            }));
        }
        Ok(())
    }

    fn definition_body_hash_opt(&self, definition_hash: &str) -> Result<Option<String>> {
        if self.definition_is_external(definition_hash)? {
            Ok(None)
        } else {
            Ok(Some(self.function_body_hash(definition_hash)?))
        }
    }
}

/// Join a child segment onto a body path, avoiding a leading separator at the root.
fn child_path(path: &str, segment: &str) -> String {
    if path.is_empty() {
        segment.to_string()
    } else {
        format!("{path}/{segment}")
    }
}

pub(crate) fn dependency_pairs(
    conn: &Connection,
    root_hash: &str,
) -> Result<BTreeSet<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT from_symbol_hash, to_symbol_hash FROM dependencies
         WHERE root_hash = ?1 ORDER BY from_symbol_hash, to_symbol_hash",
    )?;
    Ok(stmt
        .query_map(params![root_hash], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?)
}

fn export_pairs(root: &ProgramRootPayload) -> BTreeSet<(String, String)> {
    root.exports
        .iter()
        .map(|binding| (binding.symbol.clone(), binding.exported_name.clone()))
        .collect()
}

fn qualified_aliases_for(root: &ProgramRootPayload, symbol: &str) -> BTreeSet<String> {
    root.names
        .iter()
        .filter(|binding| binding.symbol == symbol && !binding.is_preferred)
        .map(|binding| format!("{}.{}", binding.module, binding.display_name))
        .collect()
}

fn type_display(root: &ProgramRootPayload, type_symbol: &str) -> String {
    preferred_type_binding(root, type_symbol)
        .map(|binding| format!("{}.{}", binding.module, binding.display_name))
        .unwrap_or_else(|| type_symbol.to_string())
}

fn members_of(definition: &TypeDefinition) -> &[TypeMemberDef] {
    match definition {
        TypeDefinition::Record { fields, .. } => fields,
        TypeDefinition::Enum { variants, .. } => variants,
    }
}

fn member_label(definition: &TypeDefinition) -> &'static str {
    match definition {
        TypeDefinition::Record { .. } => "field",
        TypeDefinition::Enum { .. } => "variant",
    }
}

fn render_type_change_text(record: &JsonValue, out: &mut String) {
    let Some(object) = record.as_object() else {
        return;
    };
    if let Some(kind) = object.get("kind").and_then(JsonValue::as_str) {
        out.push_str(&format!("{kind}:\n"));
    }
    for (key, value) in object {
        if key == "kind" {
            continue;
        }
        let rendered = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        out.push_str(&format!("  {key}: {rendered}\n"));
    }
    out.push('\n');
}

fn short_json(value: &JsonValue) -> String {
    match value {
        JsonValue::String(value) => value.clone(),
        other => canonical_json(other),
    }
}
