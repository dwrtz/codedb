use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};

const SCHEMA_SQL: &str = include_str!("../schema.sql");
const OBJECT_DOMAIN: &[u8] = b"codedb/object/v1\0";
const MIGRATION_DOMAIN: &[u8] = b"codedb/migration/v1\0";
const HISTORY_DOMAIN: &[u8] = b"codedb/history/v1\0";
const CACHE_DOMAIN: &[u8] = b"codedb/cache/v1\0";
const BYTES_DOMAIN: &[u8] = b"codedb/bytes/v1\0";
const SCHEMA_VERSION: i64 = 1;
const MAIN_BRANCH: &str = "main";
const ABI_TAG: &str = "codedb-v0-internal";
const COMPILER_VERSION: &str = concat!("codedb-", env!("CARGO_PKG_VERSION"));
const PIPELINE_VERSION: &str = "pipeline:v0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RawExpr {
    LiteralI64 {
        value: String,
    },
    LiteralBool {
        value: bool,
    },
    ParamRef {
        index: usize,
    },
    ParamName {
        name: String,
    },
    Call {
        name: String,
        args: Vec<RawExpr>,
    },
    Binary {
        op: String,
        left: Box<RawExpr>,
        right: Box<RawExpr>,
    },
    If {
        cond: Box<RawExpr>,
        #[serde(rename = "then")]
        then_expr: Box<RawExpr>,
        #[serde(rename = "else")]
        else_expr: Box<RawExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSource {
    pub module: String,
    pub name: String,
    pub params: Vec<ParamSpec>,
    pub return_type: String,
    pub body: RawExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Operation {
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
    fn kind_name(&self) -> &'static str {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProgramRootPayload {
    symbols: Vec<RootSymbolPayload>,
    names: Vec<NameBinding>,
    param_names: Vec<ParamNames>,
    metadata: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RootSymbolPayload {
    symbol: String,
    definition: String,
    signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NameBinding {
    module: String,
    display_name: String,
    symbol: String,
    is_preferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ParamNames {
    symbol: String,
    names: Vec<String>,
}

#[derive(Debug, Clone)]
struct BranchState {
    root_hash: String,
    history_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct TypeCheckResult {
    expr_hash: String,
    type_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    I64(i64),
    Bool(bool),
    Unit,
}

impl Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::I64(value) => write!(f, "{value}"),
            Value::Bool(value) => write!(f, "{value}"),
            Value::Unit => write!(f, "()"),
        }
    }
}

pub struct CodeDb {
    conn: Connection,
}

impl CodeDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self { conn })
    }

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
            report.push_str(&format!(
                "applied create_function {}\nold_root {}\nnew_root {}\nmigration {}\nhistory {}\n",
                outcome.summary,
                outcome.old_root,
                outcome.new_root,
                outcome.migration_hash,
                outcome.history_hash
            ));
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
            "rendered_source",
            &source,
        )?;
        Ok(source)
    }

    pub fn eval_main_branch(&self, function_name: &str, args: Vec<Value>) -> Result<Value> {
        let branch = self.branch(MAIN_BRANCH)?;
        self.eval_name(&branch.root_hash, function_name, args)
    }

    pub fn emit_c_main_branch(&mut self, function_name: &str) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        self.resolve_name(&branch.root_hash, "main", function_name)
            .with_context(|| format!("unknown entry function {function_name}"))?;
        let source = self.render_c(&branch.root_hash)?;
        self.write_cache_text(
            &branch.root_hash,
            "c",
            "freestanding-c",
            "c_projection",
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
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let root = self.load_root(&branch.root_hash)?;
        if self
            .resolve_name(&branch.root_hash, "main", old_name)
            .is_err()
            && self
                .resolve_name(&branch.root_hash, "main", new_name)
                .is_ok()
        {
            return Ok(format!(
                "already_applied rename_symbol main.{old_name} -> main.{new_name}\nroot {}\n",
                branch.root_hash
            ));
        }
        let symbol = self.resolve_name(&branch.root_hash, "main", old_name)?;
        let old_binding = self
            .preferred_binding(&root, &symbol)
            .ok_or_else(|| anyhow!("symbol has no preferred name {symbol}"))?;
        let op = Operation::RenameSymbol {
            module: old_binding.module.clone(),
            symbol,
            old_name: old_name.to_string(),
            new_name: new_name.to_string(),
        };
        let outcome = self.apply_and_record(branch, op)?;
        Ok(format!(
            "applied rename_symbol {}\nold_root {}\nnew_root {}\nmigration {}\nhistory {}\n",
            outcome.summary,
            outcome.old_root,
            outcome.new_root,
            outcome.migration_hash,
            outcome.history_hash
        ))
    }

    pub fn replace_body_main_branch(&mut self, name: &str, expr: &str) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self.resolve_name(&branch.root_hash, "main", name)?;
        let body = parse_expr_source(expr)?;
        let op = Operation::ReplaceFunctionBody {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            body,
        };
        let outcome = self.apply_and_record(branch, op)?;
        Ok(format!(
            "applied replace_function_body {}\nold_root {}\nnew_root {}\nmigration {}\nhistory {}\n",
            outcome.summary,
            outcome.old_root,
            outcome.new_root,
            outcome.migration_hash,
            outcome.history_hash
        ))
    }

    pub fn change_signature_main_branch(&mut self, name: &str, signature: &str) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self.resolve_name(&branch.root_hash, "main", name)?;
        let (params, return_type) = parse_signature_source(signature)?;
        let op = Operation::ChangeFunctionSignature {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            params,
            return_type,
        };
        let outcome = self.apply_and_record(branch, op)?;
        Ok(format!(
            "applied change_function_signature {}\nold_root {}\nnew_root {}\nmigration {}\nhistory {}\n",
            outcome.summary,
            outcome.old_root,
            outcome.new_root,
            outcome.migration_hash,
            outcome.history_hash
        ))
    }

    pub fn delete_symbol_main_branch(&mut self, name: &str, force: bool) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self.resolve_name(&branch.root_hash, "main", name)?;
        let op = Operation::DeleteSymbol {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            force,
        };
        let outcome = self.apply_and_record(branch, op)?;
        Ok(format!(
            "applied delete_symbol {}\nold_root {}\nnew_root {}\nmigration {}\nhistory {}\n",
            outcome.summary,
            outcome.old_root,
            outcome.new_root,
            outcome.migration_hash,
            outcome.history_hash
        ))
    }

    pub fn create_alias_main_branch(&mut self, name: &str, alias: &str) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let symbol = self.resolve_name(&branch.root_hash, "main", name)?;
        let op = Operation::CreateAlias {
            module: "main".to_string(),
            symbol,
            name: name.to_string(),
            alias: alias.to_string(),
        };
        let outcome = self.apply_and_record(branch, op)?;
        Ok(format!(
            "applied create_alias {}\nold_root {}\nnew_root {}\nmigration {}\nhistory {}\n",
            outcome.summary,
            outcome.old_root,
            outcome.new_root,
            outcome.migration_hash,
            outcome.history_hash
        ))
    }
}

#[derive(Debug)]
struct MigrationOutcome {
    old_root: String,
    new_root: String,
    migration_hash: String,
    history_hash: String,
    summary: String,
}

impl CodeDb {
    fn ensure_initialized(&mut self) -> Result<String> {
        self.insert_builtin_types()?;
        let root_hash = self.put_program_root(&ProgramRootPayload {
            symbols: vec![],
            names: vec![],
            param_names: vec![],
            metadata: BTreeMap::new(),
        })?;
        self.index_root(&root_hash)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO branches (name, root_hash, history_hash) VALUES (?1, ?2, NULL)",
            params![MAIN_BRANCH, root_hash],
        )?;
        Ok(root_hash)
    }

    fn insert_builtin_types(&mut self) -> Result<()> {
        for type_name in ["I64", "Bool", "Unit"] {
            self.put_object("Type", &json!({ "type_kind": type_name }))?;
        }
        Ok(())
    }

    fn branch(&self, name: &str) -> Result<BranchState> {
        self.conn
            .query_row(
                "SELECT root_hash, history_hash FROM branches WHERE name = ?1",
                params![name],
                |row| {
                    Ok(BranchState {
                        root_hash: row.get(0)?,
                        history_hash: row.get(1)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("branch not initialized: {name}"))
    }

    fn update_branch(&mut self, name: &str, root_hash: &str, history_hash: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO branches (name, root_hash, history_hash, updated_at)
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
             ON CONFLICT(name) DO UPDATE SET
                root_hash = excluded.root_hash,
                history_hash = excluded.history_hash,
                updated_at = CURRENT_TIMESTAMP",
            params![name, root_hash, history_hash],
        )?;
        Ok(())
    }

    fn put_object(&mut self, kind: &str, payload: &JsonValue) -> Result<String> {
        let canonical = canonical_json(payload);
        let hash = hash_object_canonical(kind, SCHEMA_VERSION, &canonical);
        self.conn.execute(
            "INSERT OR IGNORE INTO objects
             (hash, kind, schema_version, payload_json, payload_size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                hash,
                kind,
                SCHEMA_VERSION,
                canonical,
                canonical.len() as i64
            ],
        )?;
        self.refresh_edges(&hash, payload)?;
        Ok(hash)
    }

    fn refresh_edges(&mut self, parent_hash: &str, payload: &JsonValue) -> Result<()> {
        self.conn.execute(
            "DELETE FROM object_edges WHERE parent_hash = ?1",
            params![parent_hash],
        )?;
        let mut refs = Vec::new();
        extract_hash_strings(payload, &mut refs);
        let mut seen = BTreeSet::new();
        for (position, child_hash) in refs.into_iter().enumerate() {
            if !seen.insert(child_hash.clone()) {
                continue;
            }
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                params![child_hash],
                |row| row.get(0),
            )?;
            if exists && child_hash != parent_hash {
                self.conn.execute(
                    "INSERT OR IGNORE INTO object_edges
                     (parent_hash, child_hash, edge_label, edge_position)
                     VALUES (?1, ?2, 'ref', ?3)",
                    params![parent_hash, child_hash, position as i64],
                )?;
            }
        }
        Ok(())
    }

    fn get_payload(&self, hash: &str) -> Result<JsonValue> {
        let payload: String = self
            .conn
            .query_row(
                "SELECT payload_json FROM objects WHERE hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("missing object {hash}"))?;
        Ok(serde_json::from_str(&payload)?)
    }

    fn get_kind(&self, hash: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT kind FROM objects WHERE hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("missing object {hash}"))
    }

    fn load_root(&self, root_hash: &str) -> Result<ProgramRootPayload> {
        let payload = self.get_payload(root_hash)?;
        let root: ProgramRootPayload = serde_json::from_value(payload)?;
        Ok(normalize_root(root))
    }

    fn put_program_root(&mut self, root: &ProgramRootPayload) -> Result<String> {
        let normalized = normalize_root(root.clone());
        let payload = serde_json::to_value(normalized)?;
        self.put_object("ProgramRoot", &payload)
    }

    fn resolve_type(&self, ty: &str) -> Result<String> {
        match ty {
            "i64" | "I64" => Ok(type_hash_for("I64")),
            "bool" | "Bool" => Ok(type_hash_for("Bool")),
            "unit" | "Unit" | "()" => Ok(type_hash_for("Unit")),
            other => bail!("unknown type {other}"),
        }
    }

    fn type_name(&self, hash: &str) -> Result<&'static str> {
        if hash == type_hash_for("I64") {
            Ok("i64")
        } else if hash == type_hash_for("Bool") {
            Ok("bool")
        } else if hash == type_hash_for("Unit") {
            Ok("unit")
        } else {
            bail!("unknown type hash {hash}")
        }
    }

    fn put_signature(&mut self, param_types: &[String], return_type: &str) -> Result<String> {
        self.put_object(
            "FunctionSignature",
            &json!({
                "params": param_types,
                "return": return_type,
                "abi": ABI_TAG,
                "effects": [],
            }),
        )
    }

    fn signature_parts(&self, signature_hash: &str) -> Result<(Vec<String>, String)> {
        let payload = self.get_payload(signature_hash)?;
        let params = payload
            .get("params")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| anyhow!("signature missing params {signature_hash}"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("signature param must be hash"))
            })
            .collect::<Result<Vec<_>>>()?;
        let return_type = payload
            .get("return")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("signature missing return {signature_hash}"))?
            .to_string();
        Ok((params, return_type))
    }

    fn put_symbol_birth(
        &mut self,
        parent_history_hash: Option<&str>,
        birth_seed: &str,
    ) -> Result<String> {
        self.put_object(
            "SymbolBirth",
            &json!({
                "symbol_kind": "function",
                "birth_history_hash": parent_history_hash.unwrap_or("genesis"),
                "local_nonce": birth_seed,
            }),
        )
    }

    fn put_function_def(&mut self, symbol: &str, signature: &str, body: &str) -> Result<String> {
        self.put_object(
            "FunctionDef",
            &json!({
                "symbol": symbol,
                "function_sig_hash": signature,
                "typed_body_expr_hash": body,
            }),
        )
    }

    fn function_body_hash(&self, definition_hash: &str) -> Result<String> {
        let payload = self.get_payload(definition_hash)?;
        payload
            .get("typed_body_expr_hash")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("function definition missing typed_body_expr_hash"))
    }

    fn function_signature_hash(&self, definition_hash: &str) -> Result<String> {
        let payload = self.get_payload(definition_hash)?;
        payload
            .get("function_sig_hash")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("function definition missing function_sig_hash"))
    }

    fn apply_and_record(&mut self, branch: BranchState, op: Operation) -> Result<MigrationOutcome> {
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

    fn preconditions_for(&self, input_root: &str, op: &Operation) -> JsonValue {
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

    fn postconditions_for(&self, output_root: &str, op: &Operation) -> JsonValue {
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

    fn operation_summary(&self, op: &Operation) -> String {
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

    fn apply_operation_to_root(
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
    fn apply_create_function(
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

    fn apply_rename_symbol(
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

    fn apply_replace_body(
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

    fn apply_change_signature(
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

    fn apply_delete_symbol(
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

    fn apply_create_alias(
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

    fn assert_name_points(
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

    fn type_expr(
        &mut self,
        expr: &RawExpr,
        root: &ProgramRootPayload,
        param_names: &[String],
        param_types: &[String],
    ) -> Result<TypeCheckResult> {
        match expr {
            RawExpr::LiteralI64 { value } => {
                value
                    .parse::<i64>()
                    .with_context(|| format!("invalid i64 literal {value}"))?;
                let type_hash = type_hash_for("I64");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "literal_i64",
                        "value": value,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    "typed_expression",
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::LiteralBool { value } => {
                let type_hash = type_hash_for("Bool");
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "literal_bool",
                        "value": value,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    "typed_expression",
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::ParamRef { index } => {
                let type_hash = param_types
                    .get(*index)
                    .cloned()
                    .ok_or_else(|| anyhow!("parameter index out of bounds: {index}"))?;
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "param_ref",
                        "index": index,
                        "type": type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    "typed_expression",
                    &json!({ "type": type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash,
                })
            }
            RawExpr::ParamName { name } => {
                let index = param_names
                    .iter()
                    .position(|candidate| candidate == name)
                    .ok_or_else(|| anyhow!("unknown parameter {name}"))?;
                self.type_expr(&RawExpr::ParamRef { index }, root, param_names, param_types)
            }
            RawExpr::Call { name, args } => {
                let symbol = resolve_name_in_root(root, "main", name)
                    .ok_or_else(|| anyhow!("unknown function {name}"))?;
                let callee = self
                    .root_symbol(root, &symbol)
                    .ok_or_else(|| anyhow!("function {name} missing symbol entry"))?;
                let (expected_params, return_type) = self.signature_parts(&callee.signature)?;
                if expected_params.len() != args.len() {
                    bail!(
                        "call to {name} expects {} args, got {}",
                        expected_params.len(),
                        args.len()
                    );
                }
                let mut typed_args = Vec::with_capacity(args.len());
                for (idx, arg) in args.iter().enumerate() {
                    let typed = self.type_expr(arg, root, param_names, param_types)?;
                    if typed.type_hash != expected_params[idx] {
                        bail!(
                            "call arg {} for {name} expected {}, got {}",
                            idx,
                            self.type_name(&expected_params[idx])?,
                            self.type_name(&typed.type_hash)?
                        );
                    }
                    typed_args.push(typed.expr_hash);
                }
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "call",
                        "symbol": symbol,
                        "args": typed_args,
                        "type": return_type,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    "typed_expression",
                    &json!({ "type": return_type }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: return_type,
                })
            }
            RawExpr::Binary { op, left, right } => {
                let left = self.type_expr(left, root, param_names, param_types)?;
                let right = self.type_expr(right, root, param_names, param_types)?;
                let i64_hash = type_hash_for("I64");
                let bool_hash = type_hash_for("Bool");
                let result_type = match op.as_str() {
                    "+" | "-" | "*" | "/" => {
                        require_type(&left.type_hash, &i64_hash, "left operand", self)?;
                        require_type(&right.type_hash, &i64_hash, "right operand", self)?;
                        i64_hash
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        require_type(&left.type_hash, &i64_hash, "left operand", self)?;
                        require_type(&right.type_hash, &i64_hash, "right operand", self)?;
                        bool_hash
                    }
                    "&&" | "||" => {
                        require_type(&left.type_hash, &bool_hash, "left operand", self)?;
                        require_type(&right.type_hash, &bool_hash, "right operand", self)?;
                        bool_hash
                    }
                    _ => bail!("unsupported binary operator {op}"),
                };
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "binary",
                        "op": op,
                        "left": left.expr_hash,
                        "right": right.expr_hash,
                        "type": result_type,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    "typed_expression",
                    &json!({ "type": result_type }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: result_type,
                })
            }
            RawExpr::If {
                cond,
                then_expr,
                else_expr,
            } => {
                let cond = self.type_expr(cond, root, param_names, param_types)?;
                let bool_hash = type_hash_for("Bool");
                require_type(&cond.type_hash, &bool_hash, "if condition", self)?;
                let then_expr = self.type_expr(then_expr, root, param_names, param_types)?;
                let else_expr = self.type_expr(else_expr, root, param_names, param_types)?;
                if then_expr.type_hash != else_expr.type_hash {
                    bail!(
                        "if branches differ: {} vs {}",
                        self.type_name(&then_expr.type_hash)?,
                        self.type_name(&else_expr.type_hash)?
                    );
                }
                let expr_hash = self.put_object(
                    "Expression",
                    &json!({
                        "expr_kind": "if",
                        "cond": cond.expr_hash,
                        "then": then_expr.expr_hash,
                        "else": else_expr.expr_hash,
                        "type": then_expr.type_hash,
                    }),
                )?;
                self.write_cache_json(
                    &expr_hash,
                    "typechecker",
                    "typed-dag",
                    "typed_expression",
                    &json!({ "type": then_expr.type_hash }),
                )?;
                Ok(TypeCheckResult {
                    expr_hash,
                    type_hash: then_expr.type_hash,
                })
            }
        }
    }

    fn type_check_root(&self, root_hash: &str) -> Result<()> {
        let root = self.load_root(root_hash)?;
        for entry in &root.symbols {
            let (param_types, return_type) = self.signature_parts(&entry.signature)?;
            let body = self.function_body_hash(&entry.definition)?;
            let actual = self.verify_expr_type(&body, &root, &param_types)?;
            if actual != return_type {
                bail!(
                    "bad_type: function {} returns {}, body is {}",
                    self.symbol_display(&root, &entry.symbol)?,
                    self.type_name(&return_type)?,
                    self.type_name(&actual)?
                );
            }
            let definition_signature = self.function_signature_hash(&entry.definition)?;
            if definition_signature != entry.signature {
                bail!(
                    "bad_signature: root signature {} does not match definition signature {}",
                    entry.signature,
                    definition_signature
                );
            }
        }
        Ok(())
    }

    fn verify_expr_type(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        param_types: &[String],
    ) -> Result<String> {
        if self.get_kind(expr_hash)? != "Expression" {
            bail!("bad_type: object is not expression {expr_hash}");
        }
        let payload = self.get_payload(expr_hash)?;
        let declared_type = payload
            .get("type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing type {expr_hash}"))?
            .to_string();
        let actual_type = match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => type_hash_for("I64"),
            "literal_bool" => type_hash_for("Bool"),
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                param_types
                    .get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("param_ref out of bounds {index}"))?
            }
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let callee = self
                    .root_symbol(root, symbol)
                    .ok_or_else(|| anyhow!("call target missing from root {symbol}"))?;
                let (expected_params, return_type) = self.signature_parts(&callee.signature)?;
                let args = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                if args.len() != expected_params.len() {
                    bail!("call arity mismatch for {symbol}");
                }
                for (idx, arg) in args.iter().enumerate() {
                    let arg_hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    let arg_type = self.verify_expr_type(arg_hash, root, param_types)?;
                    if arg_type != expected_params[idx] {
                        bail!("call arg type mismatch for {symbol} at arg {idx}");
                    }
                }
                return_type
            }
            "binary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?;
                let left_hash = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right_hash = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                let left = self.verify_expr_type(left_hash, root, param_types)?;
                let right = self.verify_expr_type(right_hash, root, param_types)?;
                let i64_hash = type_hash_for("I64");
                let bool_hash = type_hash_for("Bool");
                match op {
                    "+" | "-" | "*" | "/" => {
                        if left != i64_hash || right != i64_hash {
                            bail!("integer op requires i64 operands");
                        }
                        i64_hash
                    }
                    "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                        if left != i64_hash || right != i64_hash {
                            bail!("comparison op requires i64 operands");
                        }
                        bool_hash
                    }
                    "&&" | "||" => {
                        if left != bool_hash || right != bool_hash {
                            bail!("bool op requires bool operands");
                        }
                        bool_hash
                    }
                    _ => bail!("unsupported binary op {op}"),
                }
            }
            "if" => {
                let cond = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing cond"))?;
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                let cond_type = self.verify_expr_type(cond, root, param_types)?;
                if cond_type != type_hash_for("Bool") {
                    bail!("if condition must be bool");
                }
                let then_type = self.verify_expr_type(then_hash, root, param_types)?;
                let else_type = self.verify_expr_type(else_hash, root, param_types)?;
                if then_type != else_type {
                    bail!("if branches must have the same type");
                }
                then_type
            }
            other => bail!("unknown expression kind {other}"),
        };
        if declared_type != actual_type {
            bail!(
                "bad_type: expression {expr_hash} declares {declared_type}, actual {actual_type}"
            );
        }
        Ok(actual_type)
    }
}

fn require_type(actual: &str, expected: &str, label: &str, db: &CodeDb) -> Result<()> {
    if actual != expected {
        bail!(
            "{label} expected {}, got {}",
            db.type_name(expected)?,
            db.type_name(actual)?
        );
    }
    Ok(())
}

impl CodeDb {
    fn eval_name(&self, root_hash: &str, function_name: &str, args: Vec<Value>) -> Result<Value> {
        let symbol = self.resolve_name(root_hash, "main", function_name)?;
        self.eval_symbol(root_hash, &symbol, args)
    }

    fn eval_symbol(&self, root_hash: &str, symbol: &str, args: Vec<Value>) -> Result<Value> {
        let root = self.load_root(root_hash)?;
        let root_symbol = self
            .root_symbol(&root, symbol)
            .ok_or_else(|| anyhow!("missing symbol {symbol}"))?;
        let (param_types, _) = self.signature_parts(&root_symbol.signature)?;
        if param_types.len() != args.len() {
            bail!(
                "{} expects {} args, got {}",
                self.symbol_display(&root, symbol)?,
                param_types.len(),
                args.len()
            );
        }
        for (idx, (arg, ty)) in args.iter().zip(param_types.iter()).enumerate() {
            match (arg, self.type_name(ty)?) {
                (Value::I64(_), "i64") | (Value::Bool(_), "bool") | (Value::Unit, "unit") => {}
                _ => bail!(
                    "argument {idx} has wrong type for {}",
                    self.symbol_display(&root, symbol)?
                ),
            }
        }
        let body = self.function_body_hash(&root_symbol.definition)?;
        self.eval_expr(root_hash, &body, &args)
    }

    fn eval_expr(&self, root_hash: &str, expr_hash: &str, args: &[Value]) -> Result<Value> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .parse::<i64>()?;
                Ok(Value::I64(value))
            }
            "literal_bool" => {
                let value = payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?;
                Ok(Value::Bool(value))
            }
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                args.get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("parameter index out of bounds: {index}"))
            }
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let arg_hashes = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                let mut call_args = Vec::with_capacity(arg_hashes.len());
                for arg in arg_hashes {
                    let hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    call_args.push(self.eval_expr(root_hash, hash, args)?);
                }
                self.eval_symbol(root_hash, symbol, call_args)
            }
            "binary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?;
                let left_hash = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right_hash = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                let left = self.eval_expr(root_hash, left_hash, args)?;
                let right = self.eval_expr(root_hash, right_hash, args)?;
                eval_binary(op, left, right)
            }
            "if" => {
                let cond_hash = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing cond"))?;
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                match self.eval_expr(root_hash, cond_hash, args)? {
                    Value::Bool(true) => self.eval_expr(root_hash, then_hash, args),
                    Value::Bool(false) => self.eval_expr(root_hash, else_hash, args),
                    other => bail!("if condition evaluated to non-bool {other}"),
                }
            }
            other => bail!("unknown expression kind {other}"),
        }
    }

    fn render_source(&self, root_hash: &str) -> Result<String> {
        let root = self.load_root(root_hash)?;
        let mut chunks = Vec::new();
        for binding in preferred_names(&root) {
            let symbol = binding.symbol;
            let root_symbol = self
                .root_symbol(&root, &symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {symbol}"))?;
            let body = self.function_body_hash(&root_symbol.definition)?;
            chunks.push(format!(
                "fn {}{} = {}",
                binding.display_name,
                self.signature_source(&root_symbol.signature, &param_names(&root, &symbol))?,
                self.expr_to_source(&body, &root, &param_names(&root, &symbol), 0)?
            ));
        }
        Ok(format!("{}\n", chunks.join("\n\n")))
    }

    fn signature_source(&self, signature_hash: &str, param_names: &[String]) -> Result<String> {
        let (params, return_type) = self.signature_parts(signature_hash)?;
        let rendered_params = params
            .iter()
            .enumerate()
            .map(|(idx, ty)| {
                let name = param_names
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| format!("p{idx}"));
                Ok(format!("{name}: {}", self.type_name(ty)?))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(format!(
            "({}) -> {}",
            rendered_params.join(", "),
            self.type_name(&return_type)?
        ))
    }

    fn expr_to_source(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        local_params: &[String],
        parent_prec: u8,
    ) -> Result<String> {
        let payload = self.get_payload(expr_hash)?;
        let rendered = match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => payload
                .get("value")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                .to_string(),
            "literal_bool" => payload
                .get("value")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| anyhow!("literal_bool missing value"))?
                .to_string(),
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                local_params
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("p{index}"))
            }
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let args = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                let rendered_args = args
                    .iter()
                    .map(|arg| {
                        let hash = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        self.expr_to_source(hash, root, local_params, 0)
                    })
                    .collect::<Result<Vec<_>>>()?;
                format!(
                    "{}({})",
                    self.symbol_display(root, symbol)?,
                    rendered_args.join(", ")
                )
            }
            "binary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?;
                let prec = op_precedence(op);
                let left = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                let expr = format!(
                    "{} {} {}",
                    self.expr_to_source(left, root, local_params, prec)?,
                    op,
                    self.expr_to_source(right, root, local_params, prec + 1)?
                );
                if prec < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "if" => {
                let cond = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing cond"))?;
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                format!(
                    "if {} then {} else {}",
                    self.expr_to_source(cond, root, local_params, 0)?,
                    self.expr_to_source(then_hash, root, local_params, 0)?,
                    self.expr_to_source(else_hash, root, local_params, 0)?
                )
            }
            other => bail!("unknown expression kind {other}"),
        };
        Ok(rendered)
    }

    fn typed_expr_to_raw(&self, expr_hash: &str, root: &ProgramRootPayload) -> Result<RawExpr> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => Ok(RawExpr::LiteralI64 {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                    .to_string(),
            }),
            "literal_bool" => Ok(RawExpr::LiteralBool {
                value: payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?,
            }),
            "param_ref" => Ok(RawExpr::ParamRef {
                index: payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize,
            }),
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let args = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                Ok(RawExpr::Call {
                    name: self.symbol_display(root, symbol)?,
                    args: args
                        .iter()
                        .map(|arg| {
                            let hash = arg
                                .as_str()
                                .ok_or_else(|| anyhow!("call arg must be hash"))?;
                            self.typed_expr_to_raw(hash, root)
                        })
                        .collect::<Result<Vec<_>>>()?,
                })
            }
            "binary" => Ok(RawExpr::Binary {
                op: payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?
                    .to_string(),
                left: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("left")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing left"))?,
                        root,
                    )?,
                ),
                right: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("right")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("binary missing right"))?,
                        root,
                    )?,
                ),
            }),
            "if" => Ok(RawExpr::If {
                cond: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("cond")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing cond"))?,
                        root,
                    )?,
                ),
                then_expr: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("then")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing then"))?,
                        root,
                    )?,
                ),
                else_expr: Box::new(
                    self.typed_expr_to_raw(
                        payload
                            .get("else")
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| anyhow!("if missing else"))?,
                        root,
                    )?,
                ),
            }),
            other => bail!("unknown expression kind {other}"),
        }
    }
}

fn eval_binary(op: &str, left: Value, right: Value) -> Result<Value> {
    match (op, left, right) {
        ("+", Value::I64(a), Value::I64(b)) => Ok(Value::I64(a + b)),
        ("-", Value::I64(a), Value::I64(b)) => Ok(Value::I64(a - b)),
        ("*", Value::I64(a), Value::I64(b)) => Ok(Value::I64(a * b)),
        ("/", Value::I64(_), Value::I64(0)) => bail!("division by zero"),
        ("/", Value::I64(a), Value::I64(b)) => Ok(Value::I64(a / b)),
        ("==", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a == b)),
        ("!=", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a != b)),
        ("<", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a < b)),
        ("<=", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a <= b)),
        (">", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a > b)),
        (">=", Value::I64(a), Value::I64(b)) => Ok(Value::Bool(a >= b)),
        ("&&", Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
        ("||", Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
        (op, left, right) => bail!("invalid operands for {op}: {left}, {right}"),
    }
}

fn op_precedence(op: &str) -> u8 {
    match op {
        "||" => 1,
        "&&" => 2,
        "==" | "!=" => 3,
        "<" | "<=" | ">" | ">=" => 4,
        "+" | "-" => 5,
        "*" | "/" => 6,
        _ => 9,
    }
}

impl CodeDb {
    fn index_root(&mut self, root_hash: &str) -> Result<()> {
        let root = self.load_root(root_hash)?;
        self.conn.execute(
            "DELETE FROM root_symbols WHERE root_hash = ?1",
            params![root_hash],
        )?;
        self.conn.execute(
            "DELETE FROM root_names WHERE root_hash = ?1",
            params![root_hash],
        )?;
        self.conn.execute(
            "DELETE FROM dependencies WHERE root_hash = ?1",
            params![root_hash],
        )?;
        self.conn.execute(
            "DELETE FROM source_search WHERE root_hash = ?1",
            params![root_hash],
        )?;

        for entry in &root.symbols {
            self.conn.execute(
                "INSERT OR REPLACE INTO root_symbols
                 (root_hash, symbol_hash, definition_hash, signature_hash)
                 VALUES (?1, ?2, ?3, ?4)",
                params![root_hash, entry.symbol, entry.definition, entry.signature],
            )?;
            self.write_cache_json(
                &entry.signature,
                "typechecker",
                "interface",
                "interface_hash",
                &json!({ "symbol": entry.symbol, "signature": entry.signature }),
            )?;
            self.write_cache_json(
                &entry.definition,
                "lowering",
                "implementation",
                "implementation_hash",
                &json!({ "symbol": entry.symbol, "definition": entry.definition }),
            )?;
        }

        for binding in &root.names {
            self.conn.execute(
                "INSERT OR REPLACE INTO root_names
                 (root_hash, module_name, display_name, symbol_hash, is_preferred)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    root_hash,
                    binding.module,
                    binding.display_name,
                    binding.symbol,
                    if binding.is_preferred { 1 } else { 0 }
                ],
            )?;
        }

        for entry in &root.symbols {
            let deps = self.dependencies_for_definition(&root, &entry.definition)?;
            self.write_cache_json(
                &entry.definition,
                "analysis",
                "dependencies",
                "function_dependency_set",
                &json!({ "dependencies": deps.iter().cloned().collect::<Vec<_>>() }),
            )?;
            for dep in deps {
                self.conn.execute(
                    "INSERT OR IGNORE INTO dependencies
                     (root_hash, from_symbol_hash, to_symbol_hash)
                     VALUES (?1, ?2, ?3)",
                    params![root_hash, entry.symbol, dep],
                )?;
            }
        }

        for binding in preferred_names(&root) {
            let symbol = binding.symbol.clone();
            if let Some(entry) = self.root_symbol(&root, &symbol) {
                let body = self.function_body_hash(&entry.definition)?;
                let source = format!(
                    "fn {}{} = {}",
                    binding.display_name,
                    self.signature_source(&entry.signature, &param_names(&root, &symbol))?,
                    self.expr_to_source(&body, &root, &param_names(&root, &symbol), 0)?
                );
                self.conn.execute(
                    "INSERT INTO source_search (root_hash, symbol_hash, rendered_source)
                     VALUES (?1, ?2, ?3)",
                    params![root_hash, symbol, source],
                )?;
            }
        }
        Ok(())
    }

    fn dependencies_for_definition(
        &self,
        root: &ProgramRootPayload,
        definition_hash: &str,
    ) -> Result<BTreeSet<String>> {
        let body = self.function_body_hash(definition_hash)?;
        let mut deps = BTreeSet::new();
        self.collect_expr_deps(root, &body, &mut deps)?;
        Ok(deps)
    }

    fn collect_expr_deps(
        &self,
        root: &ProgramRootPayload,
        expr_hash: &str,
        deps: &mut BTreeSet<String>,
    ) -> Result<()> {
        let payload = self.get_payload(expr_hash)?;
        match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" | "literal_bool" | "param_ref" => {}
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                if self.root_symbol(root, symbol).is_some() {
                    deps.insert(symbol.to_string());
                }
                for arg in payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?
                {
                    let hash = arg
                        .as_str()
                        .ok_or_else(|| anyhow!("call arg must be hash"))?;
                    self.collect_expr_deps(root, hash, deps)?;
                }
            }
            "binary" => {
                let left = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                self.collect_expr_deps(root, left, deps)?;
                self.collect_expr_deps(root, right, deps)?;
            }
            "if" => {
                for key in ["cond", "then", "else"] {
                    let child = payload
                        .get(key)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| anyhow!("if missing {key}"))?;
                    self.collect_expr_deps(root, child, deps)?;
                }
            }
            other => bail!("unknown expression kind {other}"),
        }
        Ok(())
    }

    fn dependencies_for_symbol(&self, root_hash: &str, symbol: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT to_symbol_hash FROM dependencies
             WHERE root_hash = ?1 AND from_symbol_hash = ?2 ORDER BY to_symbol_hash",
        )?;
        Ok(stmt
            .query_map(params![root_hash, symbol], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn reverse_dependencies_for_root(
        &self,
        root: &ProgramRootPayload,
        symbol: &str,
    ) -> Result<Vec<String>> {
        let mut callers = Vec::new();
        for entry in &root.symbols {
            let deps = self.dependencies_for_definition(root, &entry.definition)?;
            if deps.contains(symbol) {
                callers.push(entry.symbol.clone());
            }
        }
        callers.sort();
        Ok(callers)
    }

    fn resolve_name(&self, root_hash: &str, module: &str, name: &str) -> Result<String> {
        let root = self.load_root(root_hash)?;
        resolve_name_in_root(&root, module, name)
            .ok_or_else(|| anyhow!("unknown name {module}.{name}"))
    }

    fn resolve_symbol_or_name(&self, root_hash: &str, symbol_or_name: &str) -> Result<String> {
        if symbol_or_name.starts_with("sha256:") {
            return Ok(symbol_or_name.to_string());
        }
        self.resolve_name(root_hash, "main", symbol_or_name)
    }

    fn root_symbol<'a>(
        &self,
        root: &'a ProgramRootPayload,
        symbol: &str,
    ) -> Option<&'a RootSymbolPayload> {
        root.symbols.iter().find(|entry| entry.symbol == symbol)
    }

    fn preferred_binding<'a>(
        &self,
        root: &'a ProgramRootPayload,
        symbol: &str,
    ) -> Option<&'a NameBinding> {
        root.names
            .iter()
            .find(|binding| binding.symbol == symbol && binding.is_preferred)
            .or_else(|| root.names.iter().find(|binding| binding.symbol == symbol))
    }

    fn symbol_display(&self, root: &ProgramRootPayload, symbol: &str) -> Result<String> {
        self.preferred_binding(root, symbol)
            .map(|binding| binding.display_name.clone())
            .ok_or_else(|| anyhow!("symbol has no display name {symbol}"))
    }

    fn write_cache_text(
        &mut self,
        input_hash: &str,
        backend: &str,
        target: &str,
        artifact_kind: &str,
        text: &str,
    ) -> Result<()> {
        let artifact_hash = hash_bytes(BYTES_DOMAIN, text.as_bytes());
        let artifact_json = json!({ "text": text });
        self.write_cache(
            input_hash,
            backend,
            target,
            artifact_kind,
            &artifact_hash,
            Some(&artifact_json),
            None,
        )
    }

    fn write_cache_json(
        &mut self,
        input_hash: &str,
        backend: &str,
        target: &str,
        artifact_kind: &str,
        artifact_json: &JsonValue,
    ) -> Result<()> {
        let artifact_hash = hash_bytes(BYTES_DOMAIN, canonical_json(artifact_json).as_bytes());
        self.write_cache(
            input_hash,
            backend,
            target,
            artifact_kind,
            &artifact_hash,
            Some(artifact_json),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_cache(
        &mut self,
        input_hash: &str,
        backend: &str,
        target: &str,
        artifact_kind: &str,
        artifact_hash: &str,
        artifact_json: Option<&JsonValue>,
        artifact_bytes: Option<&[u8]>,
    ) -> Result<()> {
        let key_payload = format!(
            "{input_hash}\0{backend}\0{target}\0{COMPILER_VERSION}\0runtime:none\0{PIPELINE_VERSION}\0{artifact_kind}"
        );
        let cache_key = hash_bytes(CACHE_DOMAIN, key_payload.as_bytes());
        self.conn.execute(
            "INSERT OR REPLACE INTO compile_cache
             (cache_key, input_hash, backend, target, compiler_version, artifact_kind,
              artifact_hash, artifact_json, artifact_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                cache_key,
                input_hash,
                backend,
                target,
                COMPILER_VERSION,
                artifact_kind,
                artifact_hash,
                artifact_json.map(canonical_json),
                artifact_bytes,
            ],
        )?;
        Ok(())
    }

    fn render_c(&self, root_hash: &str) -> Result<String> {
        let root = self.load_root(root_hash)?;
        let mut out = String::new();
        out.push_str("/* generated by codedb: no allocation, no managed runtime */\n\n");
        for binding in preferred_names(&root) {
            let entry = self
                .root_symbol(&root, &binding.symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {}", binding.symbol))?;
            out.push_str(&format!(
                "{} {}({});\n",
                self.c_return_type(&entry.signature)?,
                c_symbol_name(&binding.display_name),
                self.c_param_list(&root, &binding.symbol, &entry.signature)?
            ));
        }
        out.push('\n');
        for binding in preferred_names(&root) {
            let entry = self
                .root_symbol(&root, &binding.symbol)
                .ok_or_else(|| anyhow!("root name points to missing symbol {}", binding.symbol))?;
            let body = self.function_body_hash(&entry.definition)?;
            let params = param_names(&root, &binding.symbol);
            out.push_str(&format!(
                "{} {}({}) {{\n",
                self.c_return_type(&entry.signature)?,
                c_symbol_name(&binding.display_name),
                self.c_param_list(&root, &binding.symbol, &entry.signature)?
            ));
            let return_type = self.signature_parts(&entry.signature)?.1;
            if return_type == type_hash_for("Unit") {
                out.push_str("    return;\n");
            } else {
                out.push_str(&format!(
                    "    return {};\n",
                    self.c_expr(&body, &root, &params, 0)?
                ));
            }
            out.push_str("}\n\n");
        }
        ensure_no_forbidden_runtime_calls(&out)?;
        Ok(out)
    }

    fn c_return_type(&self, signature_hash: &str) -> Result<&'static str> {
        let (_, return_type) = self.signature_parts(signature_hash)?;
        self.c_type(&return_type)
    }

    fn c_type(&self, type_hash: &str) -> Result<&'static str> {
        match self.type_name(type_hash)? {
            "i64" => Ok("long"),
            "bool" => Ok("int"),
            "unit" => Ok("void"),
            _ => unreachable!(),
        }
    }

    fn c_param_list(
        &self,
        root: &ProgramRootPayload,
        symbol: &str,
        signature_hash: &str,
    ) -> Result<String> {
        let (params, _) = self.signature_parts(signature_hash)?;
        if params.is_empty() {
            return Ok("void".to_string());
        }
        let names = param_names(root, symbol);
        params
            .iter()
            .enumerate()
            .map(|(idx, ty)| {
                Ok(format!(
                    "{} {}",
                    self.c_type(ty)?,
                    c_identifier(names.get(idx).map(String::as_str).unwrap_or("p"))
                ))
            })
            .collect::<Result<Vec<_>>>()
            .map(|items| items.join(", "))
    }

    fn c_expr(
        &self,
        expr_hash: &str,
        root: &ProgramRootPayload,
        local_params: &[String],
        parent_prec: u8,
    ) -> Result<String> {
        let payload = self.get_payload(expr_hash)?;
        let rendered = match payload
            .get("expr_kind")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("expression missing expr_kind {expr_hash}"))?
        {
            "literal_i64" => payload
                .get("value")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| anyhow!("literal_i64 missing value"))?
                .to_string(),
            "literal_bool" => {
                if payload
                    .get("value")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| anyhow!("literal_bool missing value"))?
                {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            "param_ref" => {
                let index = payload
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| anyhow!("param_ref missing index"))?
                    as usize;
                c_identifier(local_params.get(index).map(String::as_str).unwrap_or("p"))
            }
            "call" => {
                let symbol = payload
                    .get("symbol")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("call missing symbol"))?;
                let args = payload
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| anyhow!("call missing args"))?;
                let rendered_args = args
                    .iter()
                    .map(|arg| {
                        let hash = arg
                            .as_str()
                            .ok_or_else(|| anyhow!("call arg must be hash"))?;
                        self.c_expr(hash, root, local_params, 0)
                    })
                    .collect::<Result<Vec<_>>>()?;
                format!(
                    "{}({})",
                    c_symbol_name(&self.symbol_display(root, symbol)?),
                    rendered_args.join(", ")
                )
            }
            "binary" => {
                let op = payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing op"))?;
                let left = payload
                    .get("left")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing left"))?;
                let right = payload
                    .get("right")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("binary missing right"))?;
                let prec = op_precedence(op);
                let expr = format!(
                    "{} {} {}",
                    self.c_expr(left, root, local_params, prec)?,
                    op,
                    self.c_expr(right, root, local_params, prec + 1)?
                );
                if prec < parent_prec {
                    format!("({expr})")
                } else {
                    expr
                }
            }
            "if" => {
                let cond = payload
                    .get("cond")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing cond"))?;
                let then_hash = payload
                    .get("then")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing then"))?;
                let else_hash = payload
                    .get("else")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("if missing else"))?;
                format!(
                    "({} ? {} : {})",
                    self.c_expr(cond, root, local_params, 0)?,
                    self.c_expr(then_hash, root, local_params, 0)?,
                    self.c_expr(else_hash, root, local_params, 0)?
                )
            }
            other => bail!("unknown expression kind {other}"),
        };
        Ok(rendered)
    }

    pub fn diff_roots(&self, root_a: &str, root_b: &str) -> Result<String> {
        let a = self.load_root(root_a)?;
        let b = self.load_root(root_b)?;
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
                    let a_name = self.symbol_display(&a, &symbol)?;
                    let b_name = self.symbol_display(&b, &symbol)?;
                    if a_name != b_name {
                        emitted = true;
                        out.push_str("symbol_renamed:\n");
                        out.push_str(&format!(
                            "  symbol: {symbol}\n  main.{a_name} -> main.{b_name}\n"
                        ));
                        if a_entry.signature == b_entry.signature {
                            out.push_str("  signature hash: unchanged\n");
                        }
                        if self.function_body_hash(&a_entry.definition)?
                            == self.function_body_hash(&b_entry.definition)?
                        {
                            out.push_str("  function body hash: unchanged\n");
                        }
                        out.push_str("  compile impact: metadata_only\n\n");
                    }

                    let a_aliases = aliases_for(&a, &symbol);
                    let b_aliases = aliases_for(&b, &symbol);
                    for alias in b_aliases.difference(&a_aliases) {
                        emitted = true;
                        out.push_str("alias_added:\n");
                        out.push_str(&format!("  symbol: {symbol}\n  alias: main.{alias}\n\n"));
                    }
                    for alias in a_aliases.difference(&b_aliases) {
                        emitted = true;
                        out.push_str("alias_removed:\n");
                        out.push_str(&format!("  symbol: {symbol}\n  alias: main.{alias}\n\n"));
                    }

                    if a_entry.signature != b_entry.signature {
                        emitted = true;
                        out.push_str("interface_changed:\n");
                        out.push_str(&format!(
                            "  function: main.{b_name}\n  symbol: {symbol}\n  from: {}\n  to:   {}\n  compile impact: recompile dependents\n\n",
                            a_entry.signature, b_entry.signature
                        ));
                    } else if a_entry.definition != b_entry.definition {
                        emitted = true;
                        out.push_str("implementation_changed:\n");
                        out.push_str(&format!(
                            "  function: main.{b_name}\n  symbol: {symbol}\n  signature: unchanged\n  compile impact: recompile function only\n"
                        ));
                        let a_body = self.function_body_hash(&a_entry.definition)?;
                        let b_body = self.function_body_hash(&b_entry.definition)?;
                        self.diff_exprs(&a, &b, &a_body, &b_body, &mut out, "  ")?;
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

        if !emitted {
            out.push_str("Only root metadata or ordering changed.\n");
        }
        Ok(out)
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
            "param_ref" => {
                if a.get("index") != b.get("index") {
                    out.push_str(&format!(
                        "{indent}expression_replaced: param_ref {} -> {}\n",
                        short_json(a.get("index").unwrap_or(&JsonValue::Null)),
                        short_json(b.get("index").unwrap_or(&JsonValue::Null))
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

fn dependency_pairs(conn: &Connection, root_hash: &str) -> Result<BTreeSet<(String, String)>> {
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

fn short_json(value: &JsonValue) -> String {
    match value {
        JsonValue::String(value) => value.clone(),
        other => canonical_json(other),
    }
}

impl CodeDb {
    pub fn verify(&mut self) -> Result<String> {
        self.ensure_initialized()?;
        let mut errors = Vec::new();
        self.verify_objects(&mut errors)?;
        self.verify_edges(&mut errors)?;
        self.verify_branches(&mut errors)?;
        self.verify_migrations_and_histories(&mut errors)?;
        self.verify_roots(&mut errors)?;
        self.verify_caches(&mut errors)?;
        if let Err(err) = self.replay_main_branch() {
            errors.push(format!("bad_history_link: {err:#}"));
        }

        if errors.is_empty() {
            Ok("verify ok\n".to_string())
        } else {
            bail!("verify failed\n{}", errors.join("\n"));
        }
    }

    fn verify_objects(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT hash, kind, schema_version, payload_json FROM objects ORDER BY hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (hash, kind, schema_version, payload_json) = row?;
            match serde_json::from_str::<JsonValue>(&payload_json) {
                Ok(value) => {
                    let canonical = canonical_json(&value);
                    if canonical != payload_json {
                        errors.push(format!("corrupt_object: payload is not canonical {hash}"));
                    }
                    let recomputed = hash_object_canonical(&kind, schema_version, &canonical);
                    if recomputed != hash {
                        errors.push(format!("bad_hash: {hash} recomputes to {recomputed}"));
                    }
                }
                Err(err) => errors.push(format!("corrupt_object: {hash}: {err}")),
            }
        }
        Ok(())
    }

    fn verify_edges(&self, errors: &mut Vec<String>) -> Result<()> {
        let missing_parent: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM object_edges e
             LEFT JOIN objects o ON o.hash = e.parent_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        let missing_child: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM object_edges e
             LEFT JOIN objects o ON o.hash = e.child_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        if missing_parent > 0 || missing_child > 0 {
            errors.push(format!(
                "missing_object: object_edges missing parents={missing_parent} children={missing_child}"
            ));
        }
        Ok(())
    }

    fn verify_branches(&self, errors: &mut Vec<String>) -> Result<()> {
        let missing_roots: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM branches b
             LEFT JOIN objects o ON o.hash = b.root_hash
             WHERE o.hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        if missing_roots > 0 {
            errors.push(format!(
                "missing_object: branch roots missing {missing_roots}"
            ));
        }
        Ok(())
    }

    fn verify_migrations_and_histories(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT hash, parent_history_hash, input_root_hash, output_root_hash,
                    operation_json, preconditions_json, postconditions_json
             FROM migrations ORDER BY created_at, hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        for row in rows {
            let (
                hash,
                parent_history,
                input_root,
                output_root,
                operation_json,
                preconditions_json,
                postconditions_json,
            ) = row?;
            let operation = serde_json::from_str::<JsonValue>(&operation_json);
            let preconditions = serde_json::from_str::<JsonValue>(&preconditions_json);
            let postconditions = serde_json::from_str::<JsonValue>(&postconditions_json);
            match (operation, preconditions, postconditions) {
                (Ok(operation), Ok(preconditions), Ok(postconditions)) => {
                    let recomputed = migration_hash(
                        parent_history.as_deref(),
                        &input_root,
                        &output_root,
                        &operation,
                        &preconditions,
                        &postconditions,
                    );
                    if recomputed != hash {
                        errors.push(format!(
                            "bad_hash: migration {hash} recomputes to {recomputed}"
                        ));
                    }
                }
                _ => errors.push(format!("corrupt_object: migration json invalid {hash}")),
            }
            for root in [input_root, output_root] {
                let exists: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                    params![root],
                    |row| row.get(0),
                )?;
                if !exists {
                    errors.push(format!(
                        "missing_object: migration {hash} references missing root {root}"
                    ));
                }
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT history_hash, parent_history_hash, migration_hash, output_root_hash
             FROM histories ORDER BY created_at, history_hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (history, parent, migration, output_root) = row?;
            let recomputed = history_hash(parent.as_deref(), &migration, &output_root);
            if recomputed != history {
                errors.push(format!(
                    "bad_history_link: {history} recomputes to {recomputed}"
                ));
            }
        }
        Ok(())
    }

    fn verify_roots(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM objects WHERE kind = 'ProgramRoot' ORDER BY hash")?;
        let root_hashes = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        for root_hash in root_hashes {
            match self.type_check_root(&root_hash) {
                Ok(()) => {}
                Err(err) => errors.push(format!("bad_type: root {root_hash}: {err:#}")),
            }
            let root = match self.load_root(&root_hash) {
                Ok(root) => root,
                Err(err) => {
                    errors.push(format!("corrupt_object: root {root_hash}: {err:#}"));
                    continue;
                }
            };
            self.verify_root_indexes(&root_hash, &root, errors)?;
        }
        Ok(())
    }

    fn verify_root_indexes(
        &self,
        root_hash: &str,
        root: &ProgramRootPayload,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        let expected_symbols = root
            .symbols
            .iter()
            .map(|entry| {
                (
                    entry.symbol.clone(),
                    entry.definition.clone(),
                    entry.signature.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let actual_symbols = {
            let mut stmt = self.conn.prepare(
                "SELECT symbol_hash, definition_hash, signature_hash FROM root_symbols
                 WHERE root_hash = ?1 ORDER BY symbol_hash",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_symbols != actual_symbols {
            errors.push(format!("bad_index: root_symbols mismatch for {root_hash}"));
        }

        let expected_names = root
            .names
            .iter()
            .map(|binding| {
                (
                    binding.module.clone(),
                    binding.display_name.clone(),
                    binding.symbol.clone(),
                    binding.is_preferred,
                )
            })
            .collect::<BTreeSet<_>>();
        let actual_names = {
            let mut stmt = self.conn.prepare(
                "SELECT module_name, display_name, symbol_hash, is_preferred FROM root_names
                 WHERE root_hash = ?1 ORDER BY module_name, display_name",
            )?;
            stmt.query_map(params![root_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ))
            })?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        if expected_names != actual_names {
            errors.push(format!("bad_index: root_names mismatch for {root_hash}"));
        }

        let mut expected_deps = BTreeSet::new();
        for entry in &root.symbols {
            for dep in self.dependencies_for_definition(root, &entry.definition)? {
                expected_deps.insert((entry.symbol.clone(), dep));
            }
        }
        let actual_deps = dependency_pairs(&self.conn, root_hash)?;
        if expected_deps != actual_deps {
            errors.push(format!(
                "bad_dependency_index: dependencies mismatch for {root_hash}"
            ));
        }
        Ok(())
    }

    fn verify_caches(&self, errors: &mut Vec<String>) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT cache_key, input_hash, artifact_kind, artifact_json FROM compile_cache ORDER BY cache_key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (cache_key, input_hash, artifact_kind, artifact_json) = row?;
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash = ?1)",
                params![input_hash],
                |row| row.get(0),
            )?;
            if !exists {
                errors.push(format!(
                    "bad_cache_entry: {cache_key} references missing input {input_hash}"
                ));
            }
            if artifact_kind == "c_projection"
                && let Some(artifact_json) = artifact_json
            {
                match serde_json::from_str::<JsonValue>(&artifact_json) {
                    Ok(value) => {
                        if let Some(text) = value.get("text").and_then(JsonValue::as_str)
                            && let Err(err) = ensure_no_forbidden_runtime_calls(text)
                        {
                            errors.push(format!(
                                "forbidden_runtime_dependency: {cache_key}: {err:#}"
                            ));
                        }
                    }
                    Err(err) => errors.push(format!("bad_cache_entry: {cache_key}: {err}")),
                }
            }
        }
        Ok(())
    }
}

fn normalize_root(mut root: ProgramRootPayload) -> ProgramRootPayload {
    root.symbols.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    root.names.sort_by(|a, b| {
        (&a.module, &a.display_name, &a.symbol, a.is_preferred).cmp(&(
            &b.module,
            &b.display_name,
            &b.symbol,
            b.is_preferred,
        ))
    });
    root.param_names.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    root
}

fn root_symbol_index(root: &ProgramRootPayload, symbol: &str) -> Result<usize> {
    root.symbols
        .iter()
        .position(|entry| entry.symbol == symbol)
        .ok_or_else(|| anyhow!("symbol missing from root {symbol}"))
}

fn upsert_param_names(root: &mut ProgramRootPayload, symbol: &str, names: Vec<String>) {
    if let Some(entry) = root
        .param_names
        .iter_mut()
        .find(|entry| entry.symbol == symbol)
    {
        entry.names = names;
    } else {
        root.param_names.push(ParamNames {
            symbol: symbol.to_string(),
            names,
        });
    }
}

fn param_names(root: &ProgramRootPayload, symbol: &str) -> Vec<String> {
    root.param_names
        .iter()
        .find(|entry| entry.symbol == symbol)
        .map(|entry| entry.names.clone())
        .unwrap_or_default()
}

fn preferred_names(root: &ProgramRootPayload) -> Vec<NameBinding> {
    let mut names = root
        .names
        .iter()
        .filter(|binding| binding.is_preferred)
        .cloned()
        .collect::<Vec<_>>();
    names.sort_by(|a, b| {
        (&a.module, &a.display_name, &a.symbol).cmp(&(&b.module, &b.display_name, &b.symbol))
    });
    names
}

fn aliases_for(root: &ProgramRootPayload, symbol: &str) -> BTreeSet<String> {
    root.names
        .iter()
        .filter(|binding| binding.symbol == symbol && !binding.is_preferred)
        .map(|binding| binding.display_name.clone())
        .collect()
}

fn resolve_name_in_root(root: &ProgramRootPayload, module: &str, name: &str) -> Option<String> {
    root.names
        .iter()
        .find(|binding| binding.module == module && binding.display_name == name)
        .map(|binding| binding.symbol.clone())
}

fn type_hash_for(type_kind: &str) -> String {
    hash_object_canonical(
        "Type",
        SCHEMA_VERSION,
        &canonical_json(&json!({ "type_kind": type_kind })),
    )
}

fn hash_object_canonical(kind: &str, schema_version: i64, canonical_payload: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(OBJECT_DOMAIN);
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(schema_version.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_payload.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn migration_hash(
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

fn history_hash(
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

fn hash_bytes(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn canonical_json(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => serde_json::to_string(value).expect("string serialization"),
        JsonValue::Array(values) => {
            let inner = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        JsonValue::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key serialization"),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
    }
}

fn extract_hash_strings(value: &JsonValue, out: &mut Vec<String>) {
    match value {
        JsonValue::String(value) => {
            if value.starts_with("sha256:") {
                out.push(value.clone());
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                extract_hash_strings(value, out);
            }
        }
        JsonValue::Object(map) => {
            for value in map.values() {
                extract_hash_strings(value, out);
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Number(String),
    Symbol(String),
    Eof,
}

fn parse_program(source: &str) -> Result<Vec<FunctionSource>> {
    let mut parser = Parser::new(source)?;
    let mut functions = Vec::new();
    while !parser.at_eof() {
        functions.push(parser.parse_function()?);
    }
    Ok(functions)
}

fn parse_expr_source(source: &str) -> Result<RawExpr> {
    let mut parser = Parser::new(source)?;
    let expr = parser.parse_expr()?;
    parser.expect_eof()?;
    Ok(expr)
}

fn parse_signature_source(source: &str) -> Result<(Vec<ParamSpec>, String)> {
    let wrapped = format!("fn __sig__{source} = 0");
    let mut parser = Parser::new(&wrapped)?;
    let function = parser.parse_function()?;
    Ok((function.params, function.return_type))
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(source: &str) -> Result<Self> {
        Ok(Self {
            tokens: lex(source)?,
            pos: 0,
        })
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Token::Eof)
    }

    fn expect_eof(&self) -> Result<()> {
        if self.at_eof() {
            Ok(())
        } else {
            bail!("unexpected token at end: {:?}", self.peek())
        }
    }

    fn parse_function(&mut self) -> Result<FunctionSource> {
        self.expect_ident_value("fn")?;
        let name = self.expect_ident()?;
        self.expect_symbol("(")?;
        let mut params = Vec::new();
        if !self.consume_symbol(")") {
            loop {
                let param_name = self.expect_ident()?;
                self.expect_symbol(":")?;
                let ty = self.expect_ident()?;
                params.push(ParamSpec {
                    name: param_name,
                    ty,
                });
                if self.consume_symbol(")") {
                    break;
                }
                self.expect_symbol(",")?;
            }
        }
        self.expect_symbol("->")?;
        let return_type = self.expect_ident_or_unit()?;
        self.expect_symbol("=")?;
        let body = self.parse_expr()?;
        Ok(FunctionSource {
            module: "main".to_string(),
            name,
            params,
            return_type,
            body,
        })
    }

    fn parse_expr(&mut self) -> Result<RawExpr> {
        self.parse_if()
    }

    fn parse_if(&mut self) -> Result<RawExpr> {
        if self.consume_ident_value("if") {
            let cond = self.parse_expr()?;
            self.expect_ident_value("then")?;
            let then_expr = self.parse_expr()?;
            self.expect_ident_value("else")?;
            let else_expr = self.parse_expr()?;
            Ok(RawExpr::If {
                cond: Box::new(cond),
                then_expr: Box::new(then_expr),
                else_expr: Box::new(else_expr),
            })
        } else {
            self.parse_binary_prec(1)
        }
    }

    fn parse_binary_prec(&mut self, min_prec: u8) -> Result<RawExpr> {
        let mut left = self.parse_primary()?;
        loop {
            let op = match self.peek() {
                Token::Symbol(op) if is_binary_op(op) => op.clone(),
                _ => break,
            };
            let prec = op_precedence(&op);
            if prec < min_prec {
                break;
            }
            self.next();
            let right = self.parse_binary_prec(prec + 1)?;
            left = RawExpr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_primary(&mut self) -> Result<RawExpr> {
        match self.next() {
            Token::Number(value) => Ok(RawExpr::LiteralI64 { value }),
            Token::Ident(name) if name == "true" => Ok(RawExpr::LiteralBool { value: true }),
            Token::Ident(name) if name == "false" => Ok(RawExpr::LiteralBool { value: false }),
            Token::Ident(name) => {
                if self.consume_symbol("(") {
                    let mut args = Vec::new();
                    if !self.consume_symbol(")") {
                        loop {
                            args.push(self.parse_expr()?);
                            if self.consume_symbol(")") {
                                break;
                            }
                            self.expect_symbol(",")?;
                        }
                    }
                    Ok(RawExpr::Call { name, args })
                } else {
                    Ok(RawExpr::ParamName { name })
                }
            }
            Token::Symbol(symbol) if symbol == "(" => {
                let expr = self.parse_expr()?;
                self.expect_symbol(")")?;
                Ok(expr)
            }
            other => bail!("unexpected token in expression: {other:?}"),
        }
    }

    fn expect_ident(&mut self) -> Result<String> {
        match self.next() {
            Token::Ident(value) => Ok(value),
            other => bail!("expected identifier, got {other:?}"),
        }
    }

    fn expect_ident_or_unit(&mut self) -> Result<String> {
        if self.consume_symbol("(") {
            self.expect_symbol(")")?;
            Ok("unit".to_string())
        } else {
            self.expect_ident()
        }
    }

    fn expect_ident_value(&mut self, expected: &str) -> Result<()> {
        match self.next() {
            Token::Ident(value) if value == expected => Ok(()),
            other => bail!("expected {expected}, got {other:?}"),
        }
    }

    fn consume_ident_value(&mut self, expected: &str) -> bool {
        match self.peek() {
            Token::Ident(value) if value == expected => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn expect_symbol(&mut self, expected: &str) -> Result<()> {
        match self.next() {
            Token::Symbol(value) if value == expected => Ok(()),
            other => bail!("expected symbol {expected}, got {other:?}"),
        }
    }

    fn consume_symbol(&mut self, expected: &str) -> bool {
        match self.peek() {
            Token::Symbol(value) if value == expected => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn next(&mut self) -> Token {
        let token = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        if !matches!(token, Token::Eof) {
            self.pos += 1;
        }
        token
    }
}

fn lex(source: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let chars = source.chars().collect::<Vec<_>>();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
        } else if ch.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            tokens.push(Token::Number(chars[start..i].iter().collect()));
        } else if i + 1 < chars.len() {
            let two = [chars[i], chars[i + 1]].iter().collect::<String>();
            if matches!(two.as_str(), "->" | "==" | "!=" | "<=" | ">=" | "&&" | "||") {
                tokens.push(Token::Symbol(two));
                i += 2;
            } else {
                tokens.push(Token::Symbol(ch.to_string()));
                i += 1;
            }
        } else {
            tokens.push(Token::Symbol(ch.to_string()));
            i += 1;
        }
    }
    tokens.push(Token::Eof);
    Ok(tokens)
}

fn is_binary_op(op: &str) -> bool {
    matches!(
        op,
        "+" | "-" | "*" | "/" | "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||"
    )
}

fn c_symbol_name(display_name: &str) -> String {
    format!("codedb_{}", c_identifier(display_name))
}

fn c_identifier(name: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in name.chars().enumerate() {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            if idx == 0 && ch.is_ascii_digit() {
                out.push('_');
            }
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() { "_".to_string() } else { out }
}

fn ensure_no_forbidden_runtime_calls(source: &str) -> Result<()> {
    for forbidden in ["malloc", "free", "printf", "pthread_", "GC_", "dlopen"] {
        if source.contains(forbidden) {
            bail!("forbidden_runtime_dependency: generated C contains {forbidden}");
        }
    }
    Ok(())
}
