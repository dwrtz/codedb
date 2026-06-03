use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value as JsonValue, json};

use crate::expr::Value;
use crate::migrations::Operation;
use crate::model::{
    ProgramRootPayload, RootTestBinding, TEST_CASE_SCHEMA, TestCasePayload, TestCategory,
    TestValue, exports_for, test_binding_for, validate_projection_identifier,
};
use crate::store::{CodeDb, canonical_json};
use crate::{APPLE_ARM64_TARGET, DEFAULT_NATIVE_TARGET, LINUX_X86_64_TARGET, MAIN_BRANCH};

const TEST_LIST_SCHEMA: &str = "codedb/tests-list/v1";
const TEST_RUN_SCHEMA: &str = "codedb/test-run/v1";
const TEST_IMPACT_SCHEMA: &str = "codedb/test-impact/v1";

impl CodeDb {
    #[allow(clippy::too_many_arguments)]
    pub fn create_test_main_branch_expected_format(
        &mut self,
        name: &str,
        entry_name: &str,
        arg_texts: &[String],
        expected_i64: Option<&str>,
        expected_bool: Option<bool>,
        expected_unit: bool,
        category: Option<&str>,
        native_agreement: bool,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let expected = cli_expected_value(expected_i64, expected_bool, expected_unit)?;
        let op = self.create_test_operation_from_text_args(
            &operation_root,
            name,
            "main",
            entry_name,
            arg_texts,
            expected,
            parse_test_category(category)?,
            native_agreement,
        )?;
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(if json {
            outcome.format_json()
        } else {
            outcome.format_cli()
        })
    }

    pub fn delete_test_main_branch_expected_format(
        &mut self,
        name: &str,
        expected_root: Option<&str>,
        json: bool,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let operation_root = expected_root.unwrap_or(&branch.root_hash).to_string();
        let root = self.load_root(&operation_root)?;
        let test = test_binding_for(&root, name)
            .ok_or_else(|| anyhow!("unknown test {name}"))?
            .test
            .clone();
        let op = Operation::DeleteTest {
            name: name.to_string(),
            test,
        };
        let outcome = self.apply_and_record_expected(branch, &operation_root, op)?;
        Ok(if json {
            outcome.format_json()
        } else {
            outcome.format_cli()
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_test_operation_from_text_args(
        &self,
        root_hash: &str,
        name: &str,
        entry_module: &str,
        entry_name: &str,
        arg_texts: &[String],
        expected: TestValue,
        category: TestCategory,
        native_agreement: bool,
    ) -> Result<Operation> {
        let root = self.load_root(root_hash)?;
        let entry_symbol = self.resolve_name(root_hash, entry_module, entry_name)?;
        let root_symbol = self
            .root_symbol(&root, &entry_symbol)
            .ok_or_else(|| anyhow!("missing symbol {entry_symbol}"))?;
        let (param_types, _return_type) = self.signature_parts(&root_symbol.signature)?;
        if param_types.len() != arg_texts.len() {
            bail!(
                "{entry_module}.{entry_name} expects {} args, got {}",
                param_types.len(),
                arg_texts.len()
            );
        }
        let args = arg_texts
            .iter()
            .zip(param_types.iter())
            .enumerate()
            .map(|(idx, (arg, type_hash))| {
                parse_test_value_arg(arg, self.type_name(type_hash)?, idx)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Operation::CreateTest {
            name: name.to_string(),
            entry_module: entry_module.to_string(),
            entry_name: entry_name.to_string(),
            entry_symbol,
            category,
            args,
            expected,
            native_agreement,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_test_operation_from_values(
        &self,
        root_hash: &str,
        name: &str,
        entry_module: &str,
        entry_name: &str,
        entry_symbol: Option<&str>,
        args: Vec<TestValue>,
        expected: TestValue,
        category: TestCategory,
        native_agreement: bool,
    ) -> Result<Operation> {
        let symbol = match entry_symbol {
            Some(symbol) => symbol.to_string(),
            None => self.resolve_name(root_hash, entry_module, entry_name)?,
        };
        Ok(Operation::CreateTest {
            name: name.to_string(),
            entry_module: entry_module.to_string(),
            entry_name: entry_name.to_string(),
            entry_symbol: symbol,
            category,
            args,
            expected,
            native_agreement,
        })
    }

    pub(crate) fn test_hash_for_name(&self, root_hash: &str, name: &str) -> Result<String> {
        let root = self.load_root(root_hash)?;
        test_binding_for(&root, name)
            .map(|binding| binding.test.clone())
            .ok_or_else(|| anyhow!("unknown test {name}"))
    }

    pub(crate) fn put_test_case(&mut self, case: &TestCasePayload) -> Result<String> {
        if case.schema != TEST_CASE_SCHEMA {
            bail!(
                "unsupported test case schema {:?}; expected {TEST_CASE_SCHEMA}",
                case.schema
            );
        }
        self.put_object("TestCase", &serde_json::to_value(case)?)
    }

    pub(crate) fn load_test_case(&self, test_hash: &str) -> Result<TestCasePayload> {
        let kind = self.get_kind(test_hash)?;
        if kind != "TestCase" {
            bail!("object {test_hash} is {kind}, not TestCase");
        }
        let case: TestCasePayload = serde_json::from_value(self.get_payload(test_hash)?)?;
        if case.schema != TEST_CASE_SCHEMA {
            bail!(
                "unsupported test case schema {:?}; expected {TEST_CASE_SCHEMA}",
                case.schema
            );
        }
        Ok(case)
    }

    pub(crate) fn validate_test_case_for_root(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        case: &TestCasePayload,
    ) -> Result<()> {
        let entry = self
            .root_symbol(root, &case.entry_symbol)
            .ok_or_else(|| anyhow!("test entry symbol missing from root: {}", case.entry_symbol))?;
        let (param_types, return_type) = self.signature_parts(&entry.signature)?;
        if param_types.len() != case.args.len() {
            bail!(
                "test for {} expects {} args, got {}",
                self.symbol_display(root, &case.entry_symbol)?,
                param_types.len(),
                case.args.len()
            );
        }
        for (idx, (arg, type_hash)) in case.args.iter().zip(param_types.iter()).enumerate() {
            validate_test_value_type(arg, self.type_name(type_hash)?, &format!("argument {idx}"))?;
        }
        validate_test_value_type(
            &case.expected,
            self.type_name(&return_type)?,
            "expected value",
        )?;
        let args = case
            .args
            .iter()
            .map(value_from_test_value)
            .collect::<Result<Vec<_>>>()?;
        self.eval_symbol(root_hash, &case.entry_symbol, args)
            .with_context(|| format!("test entry is not evaluatable in root {root_hash}"))?;
        Ok(())
    }

    pub(crate) fn validate_tests_for_root(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
    ) -> Result<()> {
        let mut names = BTreeSet::new();
        for binding in &root.tests {
            validate_projection_identifier("test name", &binding.name)?;
            if !names.insert(binding.name.clone()) {
                bail!("duplicate test name {:?}", binding.name);
            }
            let case = self.load_test_case(&binding.test)?;
            self.validate_test_case_for_root(root_hash, root, &case)
                .with_context(|| format!("invalid test {}", binding.name))?;
        }
        Ok(())
    }

    pub fn list_tests_main_branch(&self) -> Result<String> {
        self.list_tests_branch(MAIN_BRANCH)
    }

    pub fn list_tests_main_branch_json(&self) -> Result<String> {
        self.list_tests_branch_json(MAIN_BRANCH)
    }

    pub fn list_tests_branch(&self, branch_name: &str) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let mut out = String::new();
        for binding in &root.tests {
            let case = self.load_test_case(&binding.test)?;
            out.push_str(&format!(
                "{} category {} entry {} expected {} native_agreement {}\n",
                binding.name,
                case.category.as_str(),
                self.symbol_display(&root, &case.entry_symbol)?,
                display_test_value(&case.expected),
                case.native_agreement
            ));
        }
        if out.is_empty() {
            out.push_str("tests empty\n");
        }
        Ok(out)
    }

    pub fn list_tests_branch_json(&self, branch_name: &str) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let tests = root
            .tests
            .iter()
            .map(|binding| self.test_listing_json(&root, binding))
            .collect::<Result<Vec<_>>>()?;
        Ok(format!(
            "{}\n",
            canonical_json(&json!({
                "schema": TEST_LIST_SCHEMA,
                "branch": branch_name,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
                "tests": tests,
            }))
        ))
    }

    fn test_listing_json(
        &self,
        root: &ProgramRootPayload,
        binding: &RootTestBinding,
    ) -> Result<JsonValue> {
        let case = self.load_test_case(&binding.test)?;
        Ok(json!({
            "name": binding.name,
            "test_hash": binding.test,
            "entry_name": self.symbol_display(root, &case.entry_symbol)?,
            "entry_symbol": case.entry_symbol,
            "category": case.category.as_str(),
            "args": case.args,
            "expected": case.expected,
            "native_agreement": case.native_agreement,
        }))
    }

    pub fn run_tests_main_branch(&mut self) -> Result<String> {
        self.run_tests_branch(MAIN_BRANCH)
    }

    pub fn run_tests_main_branch_json(&mut self) -> Result<String> {
        self.run_tests_branch_json(MAIN_BRANCH)
    }

    pub fn run_tests_branch(&mut self, branch_name: &str) -> Result<String> {
        let payload: JsonValue =
            serde_json::from_str(self.run_tests_branch_json(branch_name)?.trim_end())?;
        let mut out = String::new();
        for test in payload
            .get("tests")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            out.push_str(&format!(
                "{} {} reference {}\n",
                test.get("status")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("error"),
                test.get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("<unknown>"),
                test.get("reference")
                    .and_then(|reference| reference.get("status"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or("error")
            ));
        }
        out.push_str(&format!(
            "summary status {} passed {} failed {} errors {} native_skipped {}\n",
            payload
                .get("status")
                .and_then(JsonValue::as_str)
                .unwrap_or("error"),
            payload
                .get("passed")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
            payload
                .get("failed")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
            payload
                .get("errors")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
            payload
                .get("native_skipped")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
        ));
        Ok(out)
    }

    pub fn run_tests_branch_json(&mut self, branch_name: &str) -> Result<String> {
        let branch = self.branch(branch_name)?;
        let root = self.load_root(&branch.root_hash)?;
        let test_bindings = root.tests.clone();
        let mut tests = Vec::new();
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut errors = 0usize;
        let mut native_skipped = 0usize;

        for binding in &test_bindings {
            let result = self.run_one_test(branch_name, &branch.root_hash, &root, binding)?;
            match result.get("status").and_then(JsonValue::as_str) {
                Some("passed") => passed += 1,
                Some("failed") => failed += 1,
                _ => errors += 1,
            }
            if result
                .get("native_agreement")
                .and_then(|native| native.get("status"))
                .and_then(JsonValue::as_str)
                == Some("skipped")
            {
                native_skipped += 1;
            }
            tests.push(result);
        }

        let status = if errors > 0 {
            "error"
        } else if failed > 0 {
            "failed"
        } else {
            "passed"
        };
        Ok(format!(
            "{}\n",
            canonical_json(&json!({
                "schema": TEST_RUN_SCHEMA,
                "branch": branch_name,
                "root_hash": branch.root_hash,
                "history_hash": branch.history_hash,
                "status": status,
                "passed": passed,
                "failed": failed,
                "errors": errors,
                "native_skipped": native_skipped,
                "tests": tests,
            }))
        ))
    }

    fn run_one_test(
        &mut self,
        branch_name: &str,
        root_hash: &str,
        root: &ProgramRootPayload,
        binding: &RootTestBinding,
    ) -> Result<JsonValue> {
        let case = self.load_test_case(&binding.test)?;
        let entry_name = self.symbol_display(root, &case.entry_symbol)?;
        let expected = value_from_test_value(&case.expected)?;
        let args = case
            .args
            .iter()
            .map(value_from_test_value)
            .collect::<Result<Vec<_>>>()?;
        let reference_result = match self.eval_symbol(root_hash, &case.entry_symbol, args) {
            Ok(actual) => {
                let status = if actual == expected {
                    "passed"
                } else {
                    "failed"
                };
                json!({
                    "status": status,
                    "expected": &case.expected,
                    "actual": test_value_from_value(&actual),
                })
            }
            Err(err) => json!({
                "status": "error",
                "expected": &case.expected,
                "error": format!("{err:#}"),
            }),
        };
        let mut status = reference_result
            .get("status")
            .and_then(JsonValue::as_str)
            .unwrap_or("error")
            .to_string();
        let native_result =
            self.native_agreement_result(branch_name, &entry_name, &case, &expected);
        if native_result.get("status").and_then(JsonValue::as_str) == Some("failed") {
            status = "failed".to_string();
        }
        Ok(json!({
            "name": binding.name,
            "test_hash": binding.test,
            "entry_name": entry_name,
            "entry_symbol": case.entry_symbol,
            "category": case.category.as_str(),
            "args": case.args,
            "expected": case.expected,
            "status": status,
            "reference": reference_result,
            "native_agreement": native_result,
        }))
    }

    pub fn test_impact(&self, old_root_hash: &str, new_root_hash: &str) -> Result<String> {
        let payload: JsonValue = serde_json::from_str(
            self.test_impact_json(old_root_hash, new_root_hash)?
                .trim_end(),
        )?;
        let mut out = String::new();
        out.push_str(&format!(
            "test_impact selected {} skipped {}\n",
            payload
                .get("selected")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
            payload
                .get("skipped")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0)
        ));
        for test in payload
            .get("tests")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            let reason_names = test
                .get("reasons")
                .and_then(JsonValue::as_array)
                .map(|reasons| {
                    reasons
                        .iter()
                        .filter_map(|reason| reason.get("kind").and_then(JsonValue::as_str))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .filter(|reasons| !reasons.is_empty())
                .unwrap_or_else(|| "none".to_string());
            out.push_str(&format!(
                "{} {} category {} reasons {}\n",
                test.get("status")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("skipped"),
                test.get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("<unknown>"),
                test.get("category")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("behavior"),
                reason_names
            ));
        }
        Ok(out)
    }

    pub fn test_impact_json(&self, old_root_hash: &str, new_root_hash: &str) -> Result<String> {
        let old_root = self.load_root(old_root_hash)?;
        let new_root = self.load_root(new_root_hash)?;
        let build_impact = self.plan_build_impact(old_root_hash, new_root_hash)?;
        let classification =
            self.classify_test_impact_changes(old_root_hash, new_root_hash, &old_root, &new_root)?;
        let old_tests = old_root
            .tests
            .iter()
            .map(|binding| (binding.name.clone(), binding.test.clone()))
            .collect::<BTreeMap<_, _>>();

        let mut tests = Vec::new();
        let mut selected = 0usize;
        let mut skipped = 0usize;
        for binding in &new_root.tests {
            let case = self.load_test_case(&binding.test)?;
            let entry_name = self.symbol_display(&new_root, &case.entry_symbol)?;
            let reachable_symbols =
                self.reachable_symbols_from_test_entry(new_root_hash, &case.entry_symbol)?;
            let mut reasons = Vec::new();

            match old_tests.get(&binding.name) {
                None => reasons.push(impact_reason(
                    "test_added",
                    case.category,
                    Vec::new(),
                    "test did not exist in the old root",
                )),
                Some(old_test) if old_test != &binding.test => reasons.push(impact_reason(
                    "test_changed",
                    case.category,
                    Vec::new(),
                    "test object changed between roots",
                )),
                _ => {}
            }

            reasons.extend(classification.selection_reasons(case.category, &reachable_symbols));

            let is_selected = !reasons.is_empty();
            if is_selected {
                selected += 1;
            } else {
                skipped += 1;
                reasons.push(classification.skip_reason(
                    old_root_hash,
                    new_root_hash,
                    case.category,
                    &reachable_symbols,
                ));
            }

            tests.push(json!({
                "name": binding.name,
                "test_hash": binding.test,
                "entry_name": entry_name,
                "entry_symbol": case.entry_symbol,
                "category": case.category.as_str(),
                "selected": is_selected,
                "status": if is_selected { "selected" } else { "skipped" },
                "reasons": reasons,
                "reachable_symbols": reachable_symbols.into_iter().collect::<Vec<_>>(),
            }));
        }

        Ok(format!(
            "{}\n",
            canonical_json(&json!({
                "schema": TEST_IMPACT_SCHEMA,
                "old_root_hash": old_root_hash,
                "new_root_hash": new_root_hash,
                "status": "ok",
                "selected": selected,
                "skipped": skipped,
                "build_impact": build_impact.to_json(),
                "changed_symbols": classification.changed_symbols_json(&old_root, &new_root, self),
                "global_reasons": classification.global_reasons_json(),
                "tests": tests,
            }))
        ))
    }

    fn classify_test_impact_changes(
        &self,
        old_root_hash: &str,
        new_root_hash: &str,
        old_root: &ProgramRootPayload,
        new_root: &ProgramRootPayload,
    ) -> Result<TestImpactClassification> {
        let old_symbols = old_root
            .symbols
            .iter()
            .map(|entry| (entry.symbol.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let new_symbols = new_root
            .symbols
            .iter()
            .map(|entry| (entry.symbol.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let all_symbols = old_symbols
            .keys()
            .chain(new_symbols.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut classification = TestImpactClassification::default();

        for symbol in all_symbols {
            match (old_symbols.get(&symbol), new_symbols.get(&symbol)) {
                (None, Some(_)) => {
                    classification.add_symbol_reason(
                        &symbol,
                        TestCategory::Behavior,
                        "symbol_added",
                    );
                    classification.add_symbol_reason(
                        &symbol,
                        TestCategory::Projection,
                        "symbol_added",
                    );
                }
                (Some(_), None) => {
                    classification.add_symbol_reason(
                        &symbol,
                        TestCategory::Behavior,
                        "symbol_removed",
                    );
                    classification.add_symbol_reason(
                        &symbol,
                        TestCategory::Projection,
                        "symbol_removed",
                    );
                }
                (Some(old_entry), Some(new_entry)) => {
                    if old_entry.signature != new_entry.signature {
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Behavior,
                            "signature_changed",
                        );
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Projection,
                            "signature_changed",
                        );
                    }

                    let old_body = self.function_body_hash(&old_entry.definition)?;
                    let new_body = self.function_body_hash(&new_entry.definition)?;
                    if old_body != new_body {
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Behavior,
                            "body_changed",
                        );
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Projection,
                            "body_changed",
                        );
                    } else if old_entry.definition != new_entry.definition {
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Behavior,
                            "definition_changed",
                        );
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Projection,
                            "definition_changed",
                        );
                    }

                    let old_deps = self
                        .dependencies_for_symbol(old_root_hash, &symbol)?
                        .into_iter()
                        .collect::<BTreeSet<_>>();
                    let new_deps = self
                        .dependencies_for_symbol(new_root_hash, &symbol)?
                        .into_iter()
                        .collect::<BTreeSet<_>>();
                    if old_deps != new_deps {
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Behavior,
                            "dependency_set_changed",
                        );
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Projection,
                            "dependency_set_changed",
                        );
                    }

                    if name_bindings_for_symbol(old_root, &symbol)
                        != name_bindings_for_symbol(new_root, &symbol)
                    {
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Projection,
                            "name_changed",
                        );
                    }
                    if crate::model::param_names(old_root, &symbol)
                        != crate::model::param_names(new_root, &symbol)
                    {
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Projection,
                            "parameter_names_changed",
                        );
                    }
                    if exports_for(old_root, &symbol) != exports_for(new_root, &symbol) {
                        classification.add_symbol_reason(
                            &symbol,
                            TestCategory::Export,
                            "export_map_changed",
                        );
                    }
                }
                (None, None) => unreachable!(),
            }
        }

        if old_root.metadata != new_root.metadata {
            classification
                .projection_global_reasons
                .insert("root_metadata_changed");
        }
        if old_root.tests != new_root.tests {
            classification.test_registry_changed = true;
        }
        if old_root_hash != new_root_hash && classification.is_empty() {
            classification
                .behavior_global_reasons
                .insert("unclassified_root_change");
            classification
                .projection_global_reasons
                .insert("unclassified_root_change");
            classification
                .export_global_reasons
                .insert("unclassified_root_change");
        }
        Ok(classification)
    }

    fn reachable_symbols_from_test_entry(
        &self,
        root_hash: &str,
        entry_symbol: &str,
    ) -> Result<BTreeSet<String>> {
        let mut seen = BTreeSet::new();
        let mut frontier = vec![entry_symbol.to_string()];
        while let Some(symbol) = frontier.pop() {
            if !seen.insert(symbol.clone()) {
                continue;
            }
            let mut deps = self.dependencies_for_symbol(root_hash, &symbol)?;
            deps.sort_by(|a, b| b.cmp(a));
            for dep in deps {
                if !seen.contains(&dep) {
                    frontier.push(dep);
                }
            }
        }
        Ok(seen)
    }

    fn native_agreement_result(
        &mut self,
        branch_name: &str,
        entry_name: &str,
        case: &TestCasePayload,
        expected: &Value,
    ) -> JsonValue {
        if !case.native_agreement {
            return json!({ "status": "not_requested" });
        }
        if !case.args.is_empty() {
            return json!({
                "status": "skipped",
                "reason": "native executable tests require an entry with no arguments",
                "target_triple": DEFAULT_NATIVE_TARGET,
            });
        }
        let Some(expected_exit) = expected_native_exit_code(expected) else {
            return json!({
                "status": "skipped",
                "reason": "expected value cannot be represented as a native process exit status",
                "target_triple": DEFAULT_NATIVE_TARGET,
            });
        };
        if !native_target_is_host_linkable(DEFAULT_NATIVE_TARGET) {
            return json!({
                "status": "skipped",
                "reason": "default native target is not linkable on this host",
                "target_triple": DEFAULT_NATIVE_TARGET,
            });
        }
        if !host_has_cc() {
            return json!({
                "status": "skipped",
                "reason": "cc linker is not available",
                "target_triple": DEFAULT_NATIVE_TARGET,
            });
        }
        let build = match self.build_branch(branch_name, entry_name, DEFAULT_NATIVE_TARGET) {
            Ok(build) => build,
            Err(err) => {
                return json!({
                    "status": "skipped",
                    "reason": format!("native build unavailable: {err:#}"),
                    "target_triple": DEFAULT_NATIVE_TARGET,
                });
            }
        };
        let exe = native_test_executable_path(&build.artifact_hash);
        if let Err(err) =
            std::fs::write(&exe, &build.executable).and_then(|_| make_executable(&exe))
        {
            let _ = std::fs::remove_file(&exe);
            return json!({
                "status": "failed",
                "target_triple": DEFAULT_NATIVE_TARGET,
                "error": format!("failed to materialize native executable: {err}"),
            });
        }
        let output = ProcessCommand::new(&exe).status();
        let _ = std::fs::remove_file(&exe);
        match output {
            Ok(status) => {
                let actual = status.code();
                let passed = actual == Some(expected_exit);
                json!({
                    "status": if passed { "passed" } else { "failed" },
                    "target_triple": DEFAULT_NATIVE_TARGET,
                    "expected_exit_code": expected_exit,
                    "actual_exit_code": actual,
                    "executable_cache_key": build.cache_key,
                    "executable_artifact_hash": build.artifact_hash,
                })
            }
            Err(err) => json!({
                "status": "failed",
                "target_triple": DEFAULT_NATIVE_TARGET,
                "error": format!("failed to run native executable: {err}"),
            }),
        }
    }
}

#[derive(Debug, Default)]
struct TestImpactClassification {
    symbols: BTreeMap<String, SymbolImpact>,
    behavior_symbols: BTreeSet<String>,
    projection_symbols: BTreeSet<String>,
    export_symbols: BTreeSet<String>,
    behavior_global_reasons: BTreeSet<&'static str>,
    projection_global_reasons: BTreeSet<&'static str>,
    export_global_reasons: BTreeSet<&'static str>,
    test_registry_changed: bool,
}

impl TestImpactClassification {
    fn add_symbol_reason(&mut self, symbol: &str, category: TestCategory, reason: &'static str) {
        let impact = self.symbols.entry(symbol.to_string()).or_default();
        match category {
            TestCategory::Behavior => {
                self.behavior_symbols.insert(symbol.to_string());
                impact.behavior_reasons.insert(reason);
            }
            TestCategory::Projection => {
                self.projection_symbols.insert(symbol.to_string());
                impact.projection_reasons.insert(reason);
            }
            TestCategory::Export => {
                self.export_symbols.insert(symbol.to_string());
                impact.export_reasons.insert(reason);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.symbols.is_empty()
            && self.behavior_global_reasons.is_empty()
            && self.projection_global_reasons.is_empty()
            && self.export_global_reasons.is_empty()
            && !self.test_registry_changed
    }

    fn selection_reasons(
        &self,
        category: TestCategory,
        reachable_symbols: &BTreeSet<String>,
    ) -> Vec<JsonValue> {
        match category {
            TestCategory::Behavior => {
                let affected = intersection_vec(reachable_symbols, &self.behavior_symbols);
                let mut reasons = Vec::new();
                if !affected.is_empty() {
                    reasons.push(impact_reason(
                        "changed_symbol_reachable",
                        category,
                        affected,
                        "behavior-changing symbol is reachable from test entry",
                    ));
                }
                for reason in &self.behavior_global_reasons {
                    reasons.push(impact_reason(
                        reason,
                        category,
                        Vec::new(),
                        "root change could affect behavior tests",
                    ));
                }
                reasons
            }
            TestCategory::Projection => {
                let affected = intersection_vec(reachable_symbols, &self.projection_symbols);
                let mut reasons = Vec::new();
                if !affected.is_empty() {
                    reasons.push(impact_reason(
                        "projection_symbol_reachable",
                        category,
                        affected,
                        "projection-affecting symbol is reachable from test entry",
                    ));
                }
                for reason in &self.projection_global_reasons {
                    reasons.push(impact_reason(
                        reason,
                        category,
                        Vec::new(),
                        "root change could affect projection tests",
                    ));
                }
                reasons
            }
            TestCategory::Export => {
                let mut reasons = Vec::new();
                if !self.export_symbols.is_empty() {
                    reasons.push(impact_reason(
                        "export_map_changed",
                        category,
                        self.export_symbols.iter().cloned().collect(),
                        "export-map change can affect export tests",
                    ));
                }
                for reason in &self.export_global_reasons {
                    reasons.push(impact_reason(
                        reason,
                        category,
                        Vec::new(),
                        "root change could affect export tests",
                    ));
                }
                reasons
            }
        }
    }

    fn skip_reason(
        &self,
        old_root_hash: &str,
        new_root_hash: &str,
        category: TestCategory,
        reachable_symbols: &BTreeSet<String>,
    ) -> JsonValue {
        if old_root_hash == new_root_hash {
            return impact_reason(
                "root_unchanged",
                category,
                Vec::new(),
                "old and new roots are identical",
            );
        }
        let changed_symbols = match category {
            TestCategory::Behavior => &self.behavior_symbols,
            TestCategory::Projection => &self.projection_symbols,
            TestCategory::Export => &self.export_symbols,
        };
        if !changed_symbols.is_empty()
            && intersection_vec(reachable_symbols, changed_symbols).is_empty()
        {
            return impact_reason(
                "unaffected_dependency_closure",
                category,
                changed_symbols.iter().cloned().collect(),
                "changed symbols are outside the test dependency closure",
            );
        }
        impact_reason(
            "metadata_only_change",
            category,
            Vec::new(),
            "root changed, but not in a category this test asserts",
        )
    }

    fn changed_symbols_json(
        &self,
        old_root: &ProgramRootPayload,
        new_root: &ProgramRootPayload,
        db: &CodeDb,
    ) -> Vec<JsonValue> {
        self.symbols
            .iter()
            .map(|(symbol, impact)| {
                json!({
                    "symbol_hash": symbol,
                    "name": display_name_for_changed_symbol(db, old_root, new_root, symbol),
                    "categories": impact.categories(),
                    "reasons": impact.reasons_json(),
                })
            })
            .collect()
    }

    fn global_reasons_json(&self) -> JsonValue {
        json!({
            "behavior": self.behavior_global_reasons.iter().copied().collect::<Vec<_>>(),
            "projection": self.projection_global_reasons.iter().copied().collect::<Vec<_>>(),
            "export": self.export_global_reasons.iter().copied().collect::<Vec<_>>(),
            "test_registry_changed": self.test_registry_changed,
        })
    }
}

#[derive(Debug, Default)]
struct SymbolImpact {
    behavior_reasons: BTreeSet<&'static str>,
    projection_reasons: BTreeSet<&'static str>,
    export_reasons: BTreeSet<&'static str>,
}

impl SymbolImpact {
    fn categories(&self) -> Vec<&'static str> {
        let mut categories = Vec::new();
        if !self.behavior_reasons.is_empty() {
            categories.push(TestCategory::Behavior.as_str());
        }
        if !self.projection_reasons.is_empty() {
            categories.push(TestCategory::Projection.as_str());
        }
        if !self.export_reasons.is_empty() {
            categories.push(TestCategory::Export.as_str());
        }
        categories
    }

    fn reasons_json(&self) -> JsonValue {
        json!({
            "behavior": self.behavior_reasons.iter().copied().collect::<Vec<_>>(),
            "projection": self.projection_reasons.iter().copied().collect::<Vec<_>>(),
            "export": self.export_reasons.iter().copied().collect::<Vec<_>>(),
        })
    }
}

fn intersection_vec(left: &BTreeSet<String>, right: &BTreeSet<String>) -> Vec<String> {
    left.intersection(right).cloned().collect()
}

fn impact_reason(
    kind: &'static str,
    category: TestCategory,
    symbols: Vec<String>,
    message: &'static str,
) -> JsonValue {
    json!({
        "kind": kind,
        "category": category.as_str(),
        "symbols": symbols,
        "message": message,
    })
}

fn name_bindings_for_symbol(
    root: &ProgramRootPayload,
    symbol: &str,
) -> BTreeSet<(String, String, bool)> {
    root.names
        .iter()
        .filter(|binding| binding.symbol == symbol)
        .map(|binding| {
            (
                binding.module.clone(),
                binding.display_name.clone(),
                binding.is_preferred,
            )
        })
        .collect()
}

fn display_name_for_changed_symbol(
    db: &CodeDb,
    old_root: &ProgramRootPayload,
    new_root: &ProgramRootPayload,
    symbol: &str,
) -> String {
    db.symbol_display(new_root, symbol)
        .or_else(|_| db.symbol_display(old_root, symbol))
        .unwrap_or_else(|_| symbol.to_string())
}

pub(crate) fn value_from_test_value(value: &TestValue) -> Result<Value> {
    match value {
        TestValue::I64 { value } => value
            .parse::<i64>()
            .map(Value::I64)
            .with_context(|| format!("invalid i64 test value {value:?}")),
        TestValue::Bool { value } => Ok(Value::Bool(*value)),
        TestValue::Unit => Ok(Value::Unit),
    }
}

pub(crate) fn test_value_from_value(value: &Value) -> TestValue {
    match value {
        Value::I64(value) => TestValue::I64 {
            value: value.to_string(),
        },
        Value::Bool(value) => TestValue::Bool { value: *value },
        Value::Unit => TestValue::Unit,
    }
}

pub(crate) fn validate_test_value_type(
    value: &TestValue,
    type_name: &str,
    label: &str,
) -> Result<Value> {
    let parsed = value_from_test_value(value)?;
    match (&parsed, type_name) {
        (Value::I64(_), "i64") | (Value::Bool(_), "bool") | (Value::Unit, "unit") => Ok(parsed),
        _ => bail!(
            "{label} must be {type_name}, got {}",
            display_test_value(value)
        ),
    }
}

pub(crate) fn test_points_to_entry_symbol(
    db: &CodeDb,
    test_hash: &str,
    symbol: &str,
) -> Result<bool> {
    Ok(db.load_test_case(test_hash)?.entry_symbol == symbol)
}

fn cli_expected_value(
    expected_i64: Option<&str>,
    expected_bool: Option<bool>,
    expected_unit: bool,
) -> Result<TestValue> {
    let mut count = 0;
    count += usize::from(expected_i64.is_some());
    count += usize::from(expected_bool.is_some());
    count += usize::from(expected_unit);
    if count != 1 {
        bail!("create-test requires exactly one of --expect-i64, --expect-bool, or --expect-unit");
    }
    if let Some(value) = expected_i64 {
        value
            .parse::<i64>()
            .with_context(|| format!("--expect-i64 must be i64, got {value:?}"))?;
        return Ok(TestValue::I64 {
            value: value.to_string(),
        });
    }
    if let Some(value) = expected_bool {
        return Ok(TestValue::Bool { value });
    }
    Ok(TestValue::Unit)
}

fn parse_test_category(category: Option<&str>) -> Result<TestCategory> {
    match category.unwrap_or("behavior") {
        "behavior" => Ok(TestCategory::Behavior),
        "projection" => Ok(TestCategory::Projection),
        "export" => Ok(TestCategory::Export),
        other => bail!(
            "test category must be behavior, projection, or export, got {:?}",
            other
        ),
    }
}

fn parse_test_value_arg(arg: &str, type_name: &str, idx: usize) -> Result<TestValue> {
    match type_name {
        "i64" => {
            arg.parse::<i64>()
                .with_context(|| format!("argument {idx} must be i64, got {arg:?}"))?;
            Ok(TestValue::I64 {
                value: arg.to_string(),
            })
        }
        "bool" => match arg {
            "true" => Ok(TestValue::Bool { value: true }),
            "false" => Ok(TestValue::Bool { value: false }),
            _ => bail!("argument {idx} must be bool literal true or false, got {arg:?}"),
        },
        "unit" => match arg {
            "()" | "unit" => Ok(TestValue::Unit),
            _ => bail!("argument {idx} must be unit literal () or unit, got {arg:?}"),
        },
        other => bail!("unsupported parameter type {other}"),
    }
}

fn display_test_value(value: &TestValue) -> String {
    match value {
        TestValue::I64 { value } => format!("i64:{value}"),
        TestValue::Bool { value } => format!("bool:{value}"),
        TestValue::Unit => "unit:()".to_string(),
    }
}

fn expected_native_exit_code(value: &Value) -> Option<i32> {
    match value {
        Value::I64(value) => i32::try_from(*value)
            .ok()
            .filter(|value| (0..=255).contains(value)),
        Value::Bool(value) => Some(i32::from(*value)),
        Value::Unit => None,
    }
}

fn native_target_is_host_linkable(target: &str) -> bool {
    (target == LINUX_X86_64_TARGET && cfg!(all(target_os = "linux", target_arch = "x86_64")))
        || (target == APPLE_ARM64_TARGET && cfg!(all(target_os = "macos", target_arch = "aarch64")))
}

fn host_has_cc() -> bool {
    ProcessCommand::new("cc")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn native_test_executable_path(artifact_hash: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let safe_hash = artifact_hash.replace(':', "_");
    std::env::temp_dir().join(format!(
        "codedb-test-{}-{nanos}-{safe_hash}",
        std::process::id()
    ))
}

#[cfg(unix)]
fn make_executable(path: &PathBuf) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_executable(_path: &PathBuf) -> std::io::Result<()> {
    Ok(())
}
