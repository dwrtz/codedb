use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::backend::ArtifactKind;
use crate::model::{ProgramRootPayload, RootSymbolPayload, aliases_for, param_names};
use crate::store::CodeDb;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BuildImpactKind {
    MetadataOnly,
    RelinkOnly,
    RecompileSymbols,
    RecompileDependents,
    FullRebuild,
}

impl BuildImpactKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BuildImpactKind::MetadataOnly => "metadata_only",
            BuildImpactKind::RelinkOnly => "relink_only",
            BuildImpactKind::RecompileSymbols => "recompile_symbols",
            BuildImpactKind::RecompileDependents => "recompile_dependents",
            BuildImpactKind::FullRebuild => "full_rebuild",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BuildImpactReason {
    RootUnchanged,
    MetadataChanged,
    SymbolAdded,
    SymbolRemoved,
    InterfaceHashChanged,
    ImplementationHashChanged,
    BodyExpressionHashChanged,
    DependencySetChanged,
    ExportMapChanged,
    UnclassifiedRootChange,
}

impl BuildImpactReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BuildImpactReason::RootUnchanged => "root_unchanged",
            BuildImpactReason::MetadataChanged => "metadata_changed",
            BuildImpactReason::SymbolAdded => "symbol_added",
            BuildImpactReason::SymbolRemoved => "symbol_removed",
            BuildImpactReason::InterfaceHashChanged => "interface_hash_changed",
            BuildImpactReason::ImplementationHashChanged => "implementation_hash_changed",
            BuildImpactReason::BodyExpressionHashChanged => "body_expression_hash_changed",
            BuildImpactReason::DependencySetChanged => "dependency_set_changed",
            BuildImpactReason::ExportMapChanged => "export_map_changed",
            BuildImpactReason::UnclassifiedRootChange => "unclassified_root_change",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BuildImpact {
    pub(crate) kind: BuildImpactKind,
    pub(crate) artifact_kinds: Vec<ArtifactKind>,
    pub(crate) projection_artifacts: Vec<ArtifactKind>,
    pub(crate) recompile_symbols: Vec<String>,
    pub(crate) relink: bool,
    pub(crate) changed_symbols: Vec<String>,
    pub(crate) unchanged_function_defs: Vec<String>,
    pub(crate) direct_dependents: BTreeMap<String, Vec<String>>,
    pub(crate) transitive_dependents: BTreeMap<String, Vec<String>>,
    pub(crate) reasons: Vec<BuildImpactReason>,
}

impl BuildImpact {
    pub(crate) fn metadata_only() -> Self {
        Self {
            kind: BuildImpactKind::MetadataOnly,
            artifact_kinds: vec![],
            projection_artifacts: vec![],
            recompile_symbols: vec![],
            relink: false,
            changed_symbols: vec![],
            unchanged_function_defs: vec![],
            direct_dependents: BTreeMap::new(),
            transitive_dependents: BTreeMap::new(),
            reasons: vec![BuildImpactReason::RootUnchanged],
        }
    }

    pub(crate) fn to_json(&self) -> JsonValue {
        json!({
            "kind": self.kind.as_str(),
            "artifact_kinds": artifact_names_vec(&self.artifact_kinds),
            "projection_artifacts": artifact_names_vec(&self.projection_artifacts),
            "recompile": &self.recompile_symbols,
            "relink": self.relink,
            "changed_symbols": &self.changed_symbols,
            "unchanged_function_defs": &self.unchanged_function_defs,
            "direct_dependents": &self.direct_dependents,
            "transitive_dependents": &self.transitive_dependents,
            "reasons": self.reasons.iter().map(|reason| reason.as_str()).collect::<Vec<_>>(),
        })
    }

    pub(crate) fn push_cli_lines(&self, out: &mut String) {
        out.push_str(&format!("build_impact {}\n", self.kind.as_str()));
        out.push_str(&format!(
            "artifact_kinds {}\n",
            artifact_names(&self.artifact_kinds)
        ));
        out.push_str(&format!(
            "regenerate {}\n",
            artifact_names(&self.projection_artifacts)
        ));
        out.push_str(&format!(
            "recompile {}\n",
            symbol_names(&self.recompile_symbols)
        ));
        out.push_str(&format!("relink {}\n", self.relink));
        out.push_str(&format!("reasons {}\n", reason_names(&self.reasons)));
        if !self.direct_dependents.is_empty() {
            for (symbol, dependents) in &self.direct_dependents {
                out.push_str(&format!(
                    "direct_dependents {symbol} {}\n",
                    symbol_names(dependents)
                ));
            }
        }
        if !self.transitive_dependents.is_empty() {
            for (symbol, dependents) in &self.transitive_dependents {
                out.push_str(&format!(
                    "transitive_dependents {symbol} {}\n",
                    symbol_names(dependents)
                ));
            }
        }
    }
}

pub(crate) fn artifact_names(artifacts: &[ArtifactKind]) -> String {
    if artifacts.is_empty() {
        return "none".to_string();
    }
    artifacts
        .iter()
        .map(|artifact| artifact.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn artifact_names_vec(artifacts: &[ArtifactKind]) -> Vec<&'static str> {
    artifacts.iter().map(|artifact| artifact.as_str()).collect()
}

fn reason_names(reasons: &[BuildImpactReason]) -> String {
    if reasons.is_empty() {
        return "none".to_string();
    }
    reasons
        .iter()
        .map(|reason| reason.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn symbol_names(symbols: &[String]) -> String {
    if symbols.is_empty() {
        "none".to_string()
    } else {
        symbols.join(",")
    }
}

impl CodeDb {
    pub(crate) fn plan_build_impact(
        &self,
        old_root_hash: &str,
        new_root_hash: &str,
    ) -> Result<BuildImpact> {
        if old_root_hash == new_root_hash {
            return Ok(BuildImpact::metadata_only());
        }

        let old_root = self.load_root(old_root_hash)?;
        let new_root = self.load_root(new_root_hash)?;
        let old_symbols = symbol_map(&old_root);
        let new_symbols = symbol_map(&new_root);
        let all_symbols = old_symbols
            .keys()
            .chain(new_symbols.keys())
            .cloned()
            .collect::<BTreeSet<_>>();

        let mut kind = BuildImpactKind::MetadataOnly;
        let mut recompile_symbols = BTreeSet::new();
        let mut changed_symbols = BTreeSet::new();
        let mut unchanged_function_defs = BTreeSet::new();
        let mut direct_dependents = BTreeMap::new();
        let mut transitive_dependents = BTreeMap::new();
        let mut relink = false;
        let mut reasons = BTreeSet::new();

        let metadata_changed = root_metadata_changed(&old_root, &new_root);
        if metadata_changed {
            reasons.insert(BuildImpactReason::MetadataChanged);
        }
        if old_root.exports != new_root.exports {
            raise_kind(&mut kind, BuildImpactKind::RelinkOnly);
            relink = true;
            reasons.insert(BuildImpactReason::ExportMapChanged);
            for symbol in export_changed_symbols(&old_root, &new_root) {
                changed_symbols.insert(symbol);
            }
        }

        for symbol in all_symbols {
            match (old_symbols.get(&symbol), new_symbols.get(&symbol)) {
                (None, Some(entry)) => {
                    raise_kind(&mut kind, BuildImpactKind::RecompileSymbols);
                    relink = true;
                    changed_symbols.insert(symbol.clone());
                    recompile_symbols.insert(symbol.clone());
                    reasons.insert(BuildImpactReason::SymbolAdded);
                    unchanged_function_defs.remove(&entry.definition);
                }
                (Some(_entry), None) => {
                    raise_kind(&mut kind, BuildImpactKind::RelinkOnly);
                    relink = true;
                    changed_symbols.insert(symbol.clone());
                    reasons.insert(BuildImpactReason::SymbolRemoved);
                }
                (Some(old_entry), Some(new_entry)) => {
                    let old_deps = self
                        .dependencies_for_symbol(old_root_hash, &symbol)?
                        .into_iter()
                        .collect::<BTreeSet<_>>();
                    let new_deps = self
                        .dependencies_for_symbol(new_root_hash, &symbol)?
                        .into_iter()
                        .collect::<BTreeSet<_>>();
                    let old_body = self.function_body_hash(&old_entry.definition)?;
                    let new_body = self.function_body_hash(&new_entry.definition)?;

                    if old_entry.signature != new_entry.signature {
                        raise_kind(&mut kind, BuildImpactKind::RecompileDependents);
                        relink = true;
                        changed_symbols.insert(symbol.clone());
                        recompile_symbols.insert(symbol.clone());
                        reasons.insert(BuildImpactReason::InterfaceHashChanged);
                        if old_body != new_body {
                            reasons.insert(BuildImpactReason::BodyExpressionHashChanged);
                        }
                        if old_deps != new_deps {
                            reasons.insert(BuildImpactReason::DependencySetChanged);
                        }

                        let direct = self.dependents_for_signature_change(
                            old_root_hash,
                            new_root_hash,
                            &symbol,
                            &new_symbols,
                            false,
                        )?;
                        let transitive = self.dependents_for_signature_change(
                            old_root_hash,
                            new_root_hash,
                            &symbol,
                            &new_symbols,
                            true,
                        )?;
                        for dependent in &transitive {
                            recompile_symbols.insert(dependent.clone());
                        }
                        direct_dependents.insert(symbol.clone(), direct);
                        transitive_dependents.insert(symbol.clone(), transitive);
                    } else if old_entry.definition != new_entry.definition || old_deps != new_deps {
                        raise_kind(&mut kind, BuildImpactKind::RecompileSymbols);
                        relink = true;
                        changed_symbols.insert(symbol.clone());
                        recompile_symbols.insert(symbol.clone());
                        reasons.insert(BuildImpactReason::ImplementationHashChanged);
                        if old_body != new_body {
                            reasons.insert(BuildImpactReason::BodyExpressionHashChanged);
                        }
                        if old_deps != new_deps {
                            reasons.insert(BuildImpactReason::DependencySetChanged);
                        }
                    } else {
                        unchanged_function_defs.insert(new_entry.definition.clone());
                    }
                }
                (None, None) => unreachable!(),
            }
        }

        if reasons.is_empty() {
            raise_kind(&mut kind, BuildImpactKind::FullRebuild);
            reasons.insert(BuildImpactReason::UnclassifiedRootChange);
            relink = true;
        }

        let projection_artifacts = if old_root_hash == new_root_hash {
            vec![]
        } else {
            projection_artifacts()
        };
        let recompile_symbols = recompile_symbols.into_iter().collect::<Vec<_>>();
        let mut artifact_kinds = projection_artifacts.clone();
        if !recompile_symbols.is_empty() {
            artifact_kinds.push(ArtifactKind::LoweredIr);
            artifact_kinds.push(ArtifactKind::ObjectFile);
        }
        if relink {
            artifact_kinds.push(ArtifactKind::LinkPlan);
            artifact_kinds.push(ArtifactKind::Executable);
        }
        artifact_kinds.sort();
        artifact_kinds.dedup();

        let mut reasons = reasons.into_iter().collect::<Vec<_>>();
        reasons.sort();
        Ok(BuildImpact {
            kind,
            artifact_kinds,
            projection_artifacts,
            recompile_symbols,
            relink,
            changed_symbols: changed_symbols.into_iter().collect(),
            unchanged_function_defs: unchanged_function_defs.into_iter().collect(),
            direct_dependents,
            transitive_dependents,
            reasons,
        })
    }

    fn dependents_for_signature_change(
        &self,
        old_root_hash: &str,
        new_root_hash: &str,
        symbol: &str,
        new_symbols: &BTreeMap<String, &RootSymbolPayload>,
        transitive: bool,
    ) -> Result<Vec<String>> {
        let mut dependents = BTreeSet::new();
        for root_hash in [old_root_hash, new_root_hash] {
            let root_dependents = if transitive {
                self.transitive_dependents_for_symbol(root_hash, symbol)?
            } else {
                self.direct_dependents_for_symbol(root_hash, symbol)?
            };
            for dependent in root_dependents {
                if new_symbols.contains_key(&dependent) {
                    dependents.insert(dependent);
                }
            }
        }
        Ok(dependents.into_iter().collect())
    }
}

pub(crate) fn projection_artifacts() -> Vec<ArtifactKind> {
    vec![ArtifactKind::CanonicalSource, ArtifactKind::CProjection]
}

fn symbol_map(root: &ProgramRootPayload) -> BTreeMap<String, &RootSymbolPayload> {
    root.symbols
        .iter()
        .map(|entry| (entry.symbol.clone(), entry))
        .collect()
}

fn raise_kind(current: &mut BuildImpactKind, candidate: BuildImpactKind) {
    if candidate > *current {
        *current = candidate;
    }
}

fn root_metadata_changed(old_root: &ProgramRootPayload, new_root: &ProgramRootPayload) -> bool {
    old_root.names != new_root.names
        || old_root.param_names != new_root.param_names
        || old_root.tests != new_root.tests
        || old_root.metadata != new_root.metadata
        || aliases_changed(old_root, new_root)
        || param_names_changed(old_root, new_root)
}

fn export_changed_symbols(
    old_root: &ProgramRootPayload,
    new_root: &ProgramRootPayload,
) -> Vec<String> {
    let symbols = old_root
        .exports
        .iter()
        .map(|entry| entry.symbol.clone())
        .chain(new_root.exports.iter().map(|entry| entry.symbol.clone()))
        .collect::<BTreeSet<_>>();
    symbols
        .into_iter()
        .filter(|symbol| {
            old_root
                .exports
                .iter()
                .filter(|entry| entry.symbol == *symbol)
                .map(|entry| entry.exported_name.clone())
                .collect::<BTreeSet<_>>()
                != new_root
                    .exports
                    .iter()
                    .filter(|entry| entry.symbol == *symbol)
                    .map(|entry| entry.exported_name.clone())
                    .collect::<BTreeSet<_>>()
        })
        .collect()
}

fn aliases_changed(old_root: &ProgramRootPayload, new_root: &ProgramRootPayload) -> bool {
    let symbols = old_root
        .symbols
        .iter()
        .map(|entry| entry.symbol.clone())
        .chain(new_root.symbols.iter().map(|entry| entry.symbol.clone()))
        .collect::<BTreeSet<_>>();
    symbols
        .iter()
        .any(|symbol| aliases_for(old_root, symbol) != aliases_for(new_root, symbol))
}

fn param_names_changed(old_root: &ProgramRootPayload, new_root: &ProgramRootPayload) -> bool {
    let symbols = old_root
        .symbols
        .iter()
        .map(|entry| entry.symbol.clone())
        .chain(new_root.symbols.iter().map(|entry| entry.symbol.clone()))
        .collect::<BTreeSet<_>>();
    symbols
        .iter()
        .any(|symbol| param_names(old_root, symbol) != param_names(new_root, symbol))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use crate::model::{NameBinding, ProgramRootPayload, RootSymbolPayload};
    use crate::store::CodeDb;

    #[test]
    fn signature_change_recompiles_transitive_dependents_from_dependency_index() {
        let temp = tempdir().unwrap();
        let mut db = CodeDb::open(temp.path().join("plan.sqlite")).unwrap();
        db.init().unwrap();

        let i64_type = db.resolve_type("i64").unwrap();
        let bool_type = db.resolve_type("bool").unwrap();
        let sig_i64 = db
            .put_signature(std::slice::from_ref(&i64_type), &i64_type)
            .unwrap();
        let sig_bool = db
            .put_signature(std::slice::from_ref(&i64_type), &bool_type)
            .unwrap();

        let leaf_symbol = db.put_symbol_birth(None, "leaf").unwrap();
        let mid_symbol = db.put_symbol_birth(None, "mid").unwrap();
        let root_symbol = db.put_symbol_birth(None, "root").unwrap();

        let leaf_body_old = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "param_ref",
                    "index": 0,
                    "type": i64_type,
                }),
            )
            .unwrap();
        let leaf_body_new = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "literal_bool",
                    "value": true,
                    "type": bool_type,
                }),
            )
            .unwrap();
        let mid_arg = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "param_ref",
                    "index": 0,
                    "type": i64_type,
                }),
            )
            .unwrap();
        let mid_body = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "call",
                    "symbol": leaf_symbol,
                    "args": [mid_arg],
                    "type": i64_type,
                }),
            )
            .unwrap();
        let root_arg = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "literal_i64",
                    "value": "1",
                    "type": i64_type,
                }),
            )
            .unwrap();
        let root_body = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "call",
                    "symbol": mid_symbol,
                    "args": [root_arg],
                    "type": i64_type,
                }),
            )
            .unwrap();

        let leaf_def_old = db
            .put_function_def(&leaf_symbol, &sig_i64, &leaf_body_old)
            .unwrap();
        let leaf_def_new = db
            .put_function_def(&leaf_symbol, &sig_bool, &leaf_body_new)
            .unwrap();
        let mid_def = db
            .put_function_def(&mid_symbol, &sig_i64, &mid_body)
            .unwrap();
        let root_def = db
            .put_function_def(&root_symbol, &sig_i64, &root_body)
            .unwrap();

        let old_root = db
            .put_program_root(&ProgramRootPayload {
                symbols: vec![
                    RootSymbolPayload {
                        symbol: leaf_symbol.clone(),
                        definition: leaf_def_old,
                        signature: sig_i64.clone(),
                    },
                    RootSymbolPayload {
                        symbol: mid_symbol.clone(),
                        definition: mid_def.clone(),
                        signature: sig_i64.clone(),
                    },
                    RootSymbolPayload {
                        symbol: root_symbol.clone(),
                        definition: root_def.clone(),
                        signature: sig_i64.clone(),
                    },
                ],
                names: vec![
                    name("leaf", &leaf_symbol),
                    name("mid", &mid_symbol),
                    name("root", &root_symbol),
                ],
                param_names: vec![],
                exports: vec![],
                tests: vec![],
                metadata: BTreeMap::new(),
            })
            .unwrap();
        let new_root = db
            .put_program_root(&ProgramRootPayload {
                symbols: vec![
                    RootSymbolPayload {
                        symbol: leaf_symbol.clone(),
                        definition: leaf_def_new,
                        signature: sig_bool,
                    },
                    RootSymbolPayload {
                        symbol: mid_symbol.clone(),
                        definition: mid_def,
                        signature: sig_i64.clone(),
                    },
                    RootSymbolPayload {
                        symbol: root_symbol.clone(),
                        definition: root_def,
                        signature: sig_i64,
                    },
                ],
                names: vec![
                    name("leaf", &leaf_symbol),
                    name("mid", &mid_symbol),
                    name("root", &root_symbol),
                ],
                param_names: vec![],
                exports: vec![],
                tests: vec![],
                metadata: BTreeMap::new(),
            })
            .unwrap();
        db.index_root(&old_root).unwrap();
        db.index_root(&new_root).unwrap();

        let plan = db.plan_build_impact(&old_root, &new_root).unwrap();

        assert_eq!(
            plan.kind,
            crate::build_plan::BuildImpactKind::RecompileDependents
        );
        assert_eq!(
            plan.direct_dependents.get(&leaf_symbol).unwrap(),
            &vec![mid_symbol.clone()]
        );
        let transitive = plan
            .transitive_dependents
            .get(&leaf_symbol)
            .unwrap()
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            transitive,
            [mid_symbol.clone(), root_symbol.clone()]
                .into_iter()
                .collect()
        );
        let recompile = plan
            .recompile_symbols
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            recompile,
            [leaf_symbol, mid_symbol, root_symbol].into_iter().collect()
        );
    }

    #[test]
    fn signature_change_reports_dependents_from_old_dependency_graph() {
        let temp = tempdir().unwrap();
        let mut db = CodeDb::open(temp.path().join("old-dependents.sqlite")).unwrap();
        db.init().unwrap();

        let i64_type = db.resolve_type("i64").unwrap();
        let sig_one_arg = db
            .put_signature(std::slice::from_ref(&i64_type), &i64_type)
            .unwrap();
        let sig_no_args = db.put_signature(&[], &i64_type).unwrap();

        let leaf_symbol = db.put_symbol_birth(None, "leaf").unwrap();
        let mid_symbol = db.put_symbol_birth(None, "mid").unwrap();
        let root_symbol = db.put_symbol_birth(None, "root").unwrap();

        let leaf_old_body = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "param_ref",
                    "index": 0,
                    "type": i64_type,
                }),
            )
            .unwrap();
        let leaf_new_body = literal_i64(&mut db, &i64_type, "1");
        let mid_old_arg = literal_i64(&mut db, &i64_type, "2");
        let mid_old_body = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "call",
                    "symbol": leaf_symbol,
                    "args": [mid_old_arg],
                    "type": i64_type,
                }),
            )
            .unwrap();
        let mid_new_body = literal_i64(&mut db, &i64_type, "3");
        let root_old_body = db
            .put_object(
                "Expression",
                &serde_json::json!({
                    "expr_kind": "call",
                    "symbol": mid_symbol,
                    "args": [],
                    "type": i64_type,
                }),
            )
            .unwrap();
        let root_new_body = literal_i64(&mut db, &i64_type, "4");

        let leaf_def_old = db
            .put_function_def(&leaf_symbol, &sig_one_arg, &leaf_old_body)
            .unwrap();
        let leaf_def_new = db
            .put_function_def(&leaf_symbol, &sig_no_args, &leaf_new_body)
            .unwrap();
        let mid_def_old = db
            .put_function_def(&mid_symbol, &sig_no_args, &mid_old_body)
            .unwrap();
        let mid_def_new = db
            .put_function_def(&mid_symbol, &sig_no_args, &mid_new_body)
            .unwrap();
        let root_def_old = db
            .put_function_def(&root_symbol, &sig_no_args, &root_old_body)
            .unwrap();
        let root_def_new = db
            .put_function_def(&root_symbol, &sig_no_args, &root_new_body)
            .unwrap();

        let old_root = db
            .put_program_root(&ProgramRootPayload {
                symbols: vec![
                    RootSymbolPayload {
                        symbol: leaf_symbol.clone(),
                        definition: leaf_def_old,
                        signature: sig_one_arg,
                    },
                    RootSymbolPayload {
                        symbol: mid_symbol.clone(),
                        definition: mid_def_old,
                        signature: sig_no_args.clone(),
                    },
                    RootSymbolPayload {
                        symbol: root_symbol.clone(),
                        definition: root_def_old,
                        signature: sig_no_args.clone(),
                    },
                ],
                names: vec![
                    name("leaf", &leaf_symbol),
                    name("mid", &mid_symbol),
                    name("root", &root_symbol),
                ],
                param_names: vec![],
                exports: vec![],
                tests: vec![],
                metadata: BTreeMap::new(),
            })
            .unwrap();
        let new_root = db
            .put_program_root(&ProgramRootPayload {
                symbols: vec![
                    RootSymbolPayload {
                        symbol: leaf_symbol.clone(),
                        definition: leaf_def_new,
                        signature: sig_no_args.clone(),
                    },
                    RootSymbolPayload {
                        symbol: mid_symbol.clone(),
                        definition: mid_def_new,
                        signature: sig_no_args.clone(),
                    },
                    RootSymbolPayload {
                        symbol: root_symbol.clone(),
                        definition: root_def_new,
                        signature: sig_no_args,
                    },
                ],
                names: vec![
                    name("leaf", &leaf_symbol),
                    name("mid", &mid_symbol),
                    name("root", &root_symbol),
                ],
                param_names: vec![],
                exports: vec![],
                tests: vec![],
                metadata: BTreeMap::new(),
            })
            .unwrap();
        db.index_root(&old_root).unwrap();
        db.index_root(&new_root).unwrap();

        let plan = db.plan_build_impact(&old_root, &new_root).unwrap();

        assert_eq!(
            plan.direct_dependents.get(&leaf_symbol).unwrap(),
            &vec![mid_symbol.clone()]
        );
        let transitive = plan
            .transitive_dependents
            .get(&leaf_symbol)
            .unwrap()
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            transitive,
            [mid_symbol.clone(), root_symbol.clone()]
                .into_iter()
                .collect()
        );
        let recompile = plan
            .recompile_symbols
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            recompile,
            [leaf_symbol, mid_symbol, root_symbol].into_iter().collect()
        );
    }

    fn name(display_name: &str, symbol: &str) -> NameBinding {
        NameBinding {
            module: "main".to_string(),
            display_name: display_name.to_string(),
            symbol: symbol.to_string(),
            is_preferred: true,
        }
    }

    fn literal_i64(db: &mut CodeDb, i64_type: &str, value: &str) -> String {
        db.put_object(
            "Expression",
            &serde_json::json!({
                "expr_kind": "literal_i64",
                "value": value,
                "type": i64_type,
            }),
        )
        .unwrap()
    }
}
