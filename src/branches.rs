use anyhow::{Result, bail};
use serde_json::{Value as JsonValue, json};

use crate::MAIN_BRANCH;
use crate::model::BranchState;
use crate::store::{BranchFastForwardOutcome, CodeDb, canonical_json};

const BRANCH_OPERATION_RESULT_SCHEMA: &str = "codedb/branch-operation-result/v1";
const BRANCH_COMPARE_SCHEMA: &str = "codedb/branch-compare/v1";

impl CodeDb {
    pub fn create_branch_from(
        &mut self,
        name: &str,
        from: Option<&str>,
        from_root_hash: Option<&str>,
        from_history_hash: Option<&str>,
        json_format: bool,
    ) -> Result<String> {
        let (source, source_state) =
            self.branch_source_state(from, from_root_hash, from_history_hash)?;
        let created = self.create_branch_pointer(
            name,
            &source_state.root_hash,
            source_state.history_hash.as_deref(),
        )?;
        let payload = json!({
            "schema": BRANCH_OPERATION_RESULT_SCHEMA,
            "status": "created",
            "branch": name,
            "root_hash": created.root_hash,
            "history_hash": created.history_hash,
            "source": source,
        });
        if json_format {
            Ok(json_line(&payload))
        } else {
            Ok(format_branch_operation_text(&payload))
        }
    }

    pub fn fast_forward_branch(
        &mut self,
        target: &str,
        source_branch: &str,
        expected_root_hash: &str,
        json_format: bool,
    ) -> Result<String> {
        let source_state = self.branch(source_branch)?;
        let source = json!({
            "kind": "branch",
            "branch": source_branch,
            "root_hash": &source_state.root_hash,
            "history_hash": &source_state.history_hash,
        });
        let outcome =
            self.fast_forward_branch_pointer(target, expected_root_hash, &source_state)?;
        let payload = match outcome {
            BranchFastForwardOutcome::Updated { old, new } => {
                let status =
                    if old.root_hash == new.root_hash && old.history_hash == new.history_hash {
                        "already_current"
                    } else {
                        "fast_forwarded"
                    };
                json!({
                    "schema": BRANCH_OPERATION_RESULT_SCHEMA,
                    "status": status,
                    "branch": target,
                    "old_root_hash": old.root_hash,
                    "new_root_hash": new.root_hash,
                    "old_history_hash": old.history_hash,
                    "new_history_hash": new.history_hash,
                    "source": source,
                })
            }
            BranchFastForwardOutcome::StaleRoot { current } => json!({
                "schema": BRANCH_OPERATION_RESULT_SCHEMA,
                "status": "stale_root",
                "branch": target,
                "expected_root_hash": expected_root_hash,
                "actual_root_hash": current.root_hash,
                "current_history_hash": current.history_hash,
                "source": source,
            }),
            BranchFastForwardOutcome::NonFastForward { current, source: _ } => json!({
                "schema": BRANCH_OPERATION_RESULT_SCHEMA,
                "status": "non_fast_forward",
                "branch": target,
                "current_root_hash": current.root_hash,
                "current_history_hash": current.history_hash,
                "source": source,
            }),
        };
        if json_format {
            Ok(json_line(&payload))
        } else {
            Ok(format_branch_operation_text(&payload))
        }
    }

    pub fn delete_branch(&mut self, name: &str, json_format: bool) -> Result<String> {
        if name == MAIN_BRANCH {
            bail!("cannot delete the main branch");
        }
        let deleted = self.delete_branch_pointer(name)?;
        let payload = json!({
            "schema": BRANCH_OPERATION_RESULT_SCHEMA,
            "status": "deleted",
            "branch": name,
            "old_root_hash": deleted.root_hash,
            "old_history_hash": deleted.history_hash,
        });
        if json_format {
            Ok(json_line(&payload))
        } else {
            Ok(format_branch_operation_text(&payload))
        }
    }

    pub fn compare_branches(
        &self,
        branch_a: &str,
        branch_b: &str,
        json_format: bool,
    ) -> Result<String> {
        if json_format {
            self.compare_branches_json(branch_a, branch_b)
        } else {
            let state_a = self.branch(branch_a)?;
            let state_b = self.branch(branch_b)?;
            let mut out = String::new();
            out.push_str(&format!("branch_compare {branch_a} {branch_b}\n"));
            out.push_str(&format!("branch_a_root {}\n", state_a.root_hash));
            out.push_str(&format!(
                "branch_a_history {}\n",
                state_a.history_hash.as_deref().unwrap_or("none")
            ));
            out.push_str(&format!("branch_b_root {}\n", state_b.root_hash));
            out.push_str(&format!(
                "branch_b_history {}\n",
                state_b.history_hash.as_deref().unwrap_or("none")
            ));
            out.push_str(&format!(
                "same_root {}\n",
                state_a.root_hash == state_b.root_hash
            ));
            out.push_str(&format!(
                "same_history {}\n\n",
                state_a.history_hash == state_b.history_hash
            ));
            out.push_str(&self.diff_roots(&state_a.root_hash, &state_b.root_hash)?);
            Ok(out)
        }
    }

    pub fn compare_branches_json(&self, branch_a: &str, branch_b: &str) -> Result<String> {
        let state_a = self.branch(branch_a)?;
        let state_b = self.branch(branch_b)?;
        let diff: JsonValue = serde_json::from_str(
            self.diff_roots_json(&state_a.root_hash, &state_b.root_hash)?
                .trim_end(),
        )?;
        let payload = json!({
            "schema": BRANCH_COMPARE_SCHEMA,
            "branch_a": branch_state_json(branch_a, &state_a),
            "branch_b": branch_state_json(branch_b, &state_b),
            "same_root": state_a.root_hash == state_b.root_hash,
            "same_history": state_a.history_hash == state_b.history_hash,
            "old_root_hash": diff.get("old_root_hash").cloned().unwrap_or(JsonValue::Null),
            "new_root_hash": diff.get("new_root_hash").cloned().unwrap_or(JsonValue::Null),
            "changes": diff.get("changes").cloned().unwrap_or_else(|| json!([])),
            "build_impact": diff.get("build_impact").cloned().unwrap_or(JsonValue::Null),
        });
        Ok(json_line(&payload))
    }

    fn branch_source_state(
        &self,
        from: Option<&str>,
        from_root_hash: Option<&str>,
        from_history_hash: Option<&str>,
    ) -> Result<(JsonValue, BranchState)> {
        let mut source_branch = from;
        let mut source_root_hash = from_root_hash;
        if source_root_hash.is_none()
            && let Some(source) = source_branch
            && source.starts_with("sha256:")
        {
            source_root_hash = Some(source);
            source_branch = None;
        }
        if source_branch.is_some() && source_root_hash.is_some() {
            bail!("branch source must use either --from or --from-root, not both");
        }

        if let Some(root_hash) = source_root_hash {
            self.load_root(root_hash)?;
            let history_hash = from_history_hash.map(str::to_string);
            let source = json!({
                "kind": "root",
                "root_hash": root_hash,
                "history_hash": &history_hash,
            });
            return Ok((
                source,
                BranchState {
                    root_hash: root_hash.to_string(),
                    history_hash,
                },
            ));
        }

        if from_history_hash.is_some() {
            bail!("--from-history can only be used with --from-root or a sha256: --from value");
        }

        let branch = source_branch.unwrap_or(MAIN_BRANCH);
        let state = self.branch(branch)?;
        let source = json!({
            "kind": "branch",
            "branch": branch,
            "root_hash": &state.root_hash,
            "history_hash": &state.history_hash,
        });
        Ok((source, state))
    }
}

fn branch_state_json(name: &str, state: &BranchState) -> JsonValue {
    json!({
        "name": name,
        "root_hash": &state.root_hash,
        "history_hash": &state.history_hash,
    })
}

fn json_line(value: &JsonValue) -> String {
    format!("{}\n", canonical_json(value))
}

fn format_branch_operation_text(value: &JsonValue) -> String {
    let status = value
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown");
    let branch = value
        .get("branch")
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown");
    let mut out = format!("{status} branch {branch}\n");
    for key in [
        "root_hash",
        "history_hash",
        "old_root_hash",
        "new_root_hash",
        "old_history_hash",
        "new_history_hash",
        "expected_root_hash",
        "actual_root_hash",
        "current_history_hash",
    ] {
        if let Some(value) = value.get(key) {
            out.push_str(&format!("{key} {}\n", display_json_scalar(value)));
        }
    }
    if let Some(source) = value.get("source").and_then(JsonValue::as_object) {
        match source.get("kind").and_then(JsonValue::as_str) {
            Some("branch") => {
                if let Some(name) = source.get("branch").and_then(JsonValue::as_str) {
                    out.push_str(&format!("source branch {name}\n"));
                }
            }
            Some("root") => {
                if let Some(root_hash) = source.get("root_hash").and_then(JsonValue::as_str) {
                    out.push_str(&format!("source root {root_hash}\n"));
                }
            }
            _ => {}
        }
    }
    out
}

fn display_json_scalar(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "none".to_string(),
        JsonValue::String(value) => value.clone(),
        other => other.to_string(),
    }
}
