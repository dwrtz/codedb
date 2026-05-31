# SPEC.md — CodeDB Proof of Concept

Status: Draft 0.1  
Scope: first implementation / proof of concept  
Working name: `codedb`

## 1. Thesis

A program is not primarily a set of files. A program is an immutable, content-addressed semantic DAG plus a migration history that explains how one root state became another.

Files are optional human-readable projections. They are not the source of truth.

The canonical project identity is:

```text
root_hash + migration_history_head
```

Where:

- `root_hash` identifies the complete current program state.
- `migration_history_head` identifies the ordered/replayable history that produced that state.
- SQLite stores the object DAG, migration log, indexes, and caches.
- Agents write code through structural operations, not primarily through text files.

## 2. First implementation goal

Build a minimal database-backed language runtime that can:

1. Store a tiny program as a content-addressed DAG in SQLite.
2. Represent the current program state as a `ProgramRoot` object hash.
3. Apply idempotent semantic migrations that transform one root into another.
4. Rebuild database state from genesis plus migration history.
5. Generate readable source projections from the current root.
6. Produce semantic diffs between two roots.
7. Run a tiny interpreter directly from the database.
8. Cache compilation/evaluation artifacts by content hash.

The first implementation should prove the storage and migration model, not language richness.

## 3. Non-goals for v0

The first implementation does not attempt to provide:

- A production language.
- A hand-written developer editing experience.
- A full LSP.
- A custom projectional editor.
- A package manager.
- A production type system.
- A native-code backend.
- Git-style merge support.
- Perfect textual round-tripping.
- Comment/whitespace preservation.

LLVM, MLIR, Go generation, native compilation, and richer language features are explicitly post-v0.

## 4. Implementation stack

Recommended v0 stack:

```text
Language:       Python 3.12+
Database:       SQLite
Hashing:        SHA-256 for v0, BLAKE3 later if desired
Serialization:  canonical JSON
CLI:            argparse or Typer
Parser:         small handwritten parser or Lark
Runtime:        tree-walking interpreter
```

Python is recommended for the first proof of concept because the hard questions are data modeling, hashing, migrations, diffs, and invalidation. Native-code performance is not relevant yet.

## 5. Core concepts

### 5.1 Object

An object is an immutable content-addressed record.

Every object has:

```text
hash
kind
schema_version
payload
```

The object hash is computed from the object kind, schema version, and canonical payload.

```text
object_hash = sha256(
  "codedb/object/v1\0" || kind || "\0" || schema_version || "\0" || canonical_json(payload)
)
```

The exact hash algorithm may change later, but v0 must use one deterministic algorithm everywhere.

### 5.2 Program root

A `ProgramRoot` object represents a complete program state.

It points to:

- the current symbol-to-definition map,
- name metadata,
- module metadata,
- optional projection metadata.

Changing a definition, name, module assignment, or metadata object produces a new `ProgramRoot` hash.

### 5.3 Migration history

A migration is a semantic operation that transforms one root into another.

Examples:

```text
create_function
rename_symbol
replace_function_body
delete_symbol
add_test
set_metadata
```

A migration records:

```text
migration_hash
parent_history_hash
input_root_hash
output_root_hash
operation
preconditions
postconditions
agent_metadata
created_at
```

A history head is itself content-addressed:

```text
history_hash = sha256(
  "codedb/history/v1\0" || parent_history_hash || migration_hash || output_root_hash
)
```

### 5.4 Symbol identity

Global symbol names are metadata.

A symbol's stable identity is a content hash of an immutable `Symbol` object. The symbol object does not include the current display name and does not include the current function body.

Example `Symbol` payload:

```json
{
  "symbol_kind": "function",
  "birth_seed": "agent-supplied-or-deterministic-seed"
}
```

The symbol hash is stable across renames and implementation changes.

This gives three separate concepts:

```text
symbol_hash      stable identity of the symbol
name             display metadata for humans/projections
function_def     current implementation bound to that symbol in a specific root
```

### 5.5 Definition identity

A function definition is also content-addressed.

A function definition hash changes when its semantic implementation changes.

A `FunctionDef` object should contain:

```text
symbol_hash
arity
body_expr_hash
optional interface/type hash
```

Display names for the function and parameters belong in metadata, not in the semantic definition.

### 5.6 Expressions form a DAG

Expressions are immutable content-addressed objects.

They may reference other expressions by hash. Therefore, the internal representation is a DAG, not necessarily a tree.

This allows:

- expression reuse,
- shared subexpressions,
- cheap equality checks,
- hash-pruned diffs,
- incremental compilation,
- cached evaluation/lowering.

## 6. v0 language

The v0 language is deliberately tiny.

### 6.1 Top-level construct

Only top-level functions are required.

Example projection:

```text
def tax(subtotal) = subtotal * 0.20

def total(subtotal) = subtotal + tax(subtotal)

def main() = total(100)
```

### 6.2 Expression forms

Required v0 expression kinds:

```text
literal_number
literal_bool
param_ref
call
binary
if
```

Optional after the basic demo:

```text
literal_string
let
record
field_access
list
```

### 6.3 Expression object examples

Number literal:

```json
{
  "kind": "literal_number",
  "value": "100"
}
```

Parameter reference:

```json
{
  "kind": "param_ref",
  "index": 0
}
```

Function call:

```json
{
  "kind": "call",
  "symbol": "sha256:...",
  "args": ["sha256:..."]
}
```

Binary expression:

```json
{
  "kind": "binary",
  "op": "+",
  "left": "sha256:...",
  "right": "sha256:..."
}
```

Conditional:

```json
{
  "kind": "if",
  "cond": "sha256:...",
  "then": "sha256:...",
  "else": "sha256:..."
}
```

### 6.4 Name resolution

Text projections use names. The database uses symbol hashes.

During import from text:

```text
names -> symbol_hashes
```

During export to text:

```text
symbol_hashes -> preferred names
```

If two names point to the same symbol, export chooses the preferred name from root metadata.

## 7. SQLite schema

File: `schema.sql`

```sql
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS objects (
    hash TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    payload_size_bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS object_edges (
    parent_hash TEXT NOT NULL,
    child_hash TEXT NOT NULL,
    edge_label TEXT NOT NULL,
    edge_position INTEGER,
    PRIMARY KEY (parent_hash, child_hash, edge_label, edge_position),
    FOREIGN KEY (parent_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (child_hash) REFERENCES objects(hash) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS migrations (
    hash TEXT PRIMARY KEY,
    parent_history_hash TEXT,
    input_root_hash TEXT NOT NULL,
    output_root_hash TEXT NOT NULL,
    operation_kind TEXT NOT NULL,
    operation_json TEXT NOT NULL,
    preconditions_json TEXT NOT NULL,
    postconditions_json TEXT NOT NULL,
    agent_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS histories (
    history_hash TEXT PRIMARY KEY,
    parent_history_hash TEXT,
    migration_hash TEXT NOT NULL,
    output_root_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (migration_hash) REFERENCES migrations(hash) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS branches (
    name TEXT PRIMARY KEY,
    root_hash TEXT NOT NULL,
    history_hash TEXT,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (root_hash) REFERENCES objects(hash)
);

CREATE TABLE IF NOT EXISTS root_symbols (
    root_hash TEXT NOT NULL,
    symbol_hash TEXT NOT NULL,
    definition_hash TEXT NOT NULL,
    PRIMARY KEY (root_hash, symbol_hash),
    FOREIGN KEY (root_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (symbol_hash) REFERENCES objects(hash),
    FOREIGN KEY (definition_hash) REFERENCES objects(hash)
);

CREATE TABLE IF NOT EXISTS root_names (
    root_hash TEXT NOT NULL,
    module_name TEXT NOT NULL,
    display_name TEXT NOT NULL,
    symbol_hash TEXT NOT NULL,
    is_preferred INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (root_hash, module_name, display_name),
    FOREIGN KEY (root_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (symbol_hash) REFERENCES objects(hash)
);

CREATE TABLE IF NOT EXISTS dependencies (
    root_hash TEXT NOT NULL,
    from_symbol_hash TEXT NOT NULL,
    to_symbol_hash TEXT NOT NULL,
    PRIMARY KEY (root_hash, from_symbol_hash, to_symbol_hash),
    FOREIGN KEY (root_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (from_symbol_hash) REFERENCES objects(hash),
    FOREIGN KEY (to_symbol_hash) REFERENCES objects(hash)
);

CREATE TABLE IF NOT EXISTS compile_cache (
    cache_key TEXT PRIMARY KEY,
    input_hash TEXT NOT NULL,
    backend TEXT NOT NULL,
    target TEXT NOT NULL,
    compiler_version TEXT NOT NULL,
    artifact_kind TEXT NOT NULL,
    artifact_hash TEXT NOT NULL,
    artifact_json TEXT,
    artifact_bytes BLOB,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (input_hash) REFERENCES objects(hash)
);

CREATE VIRTUAL TABLE IF NOT EXISTS source_search
USING fts5(root_hash, symbol_hash, rendered_source);
```

Notes:

- `objects` is canonical storage.
- `object_edges` supports graph traversal and garbage collection.
- `root_symbols`, `root_names`, and `dependencies` are materialized indexes. They can be rebuilt from `objects` and the current root.
- `compile_cache` is disposable and can be invalidated/rebuilt.
- `source_search` is a convenience index for projections and debugging.

## 8. Canonical JSON

All object payloads must be serialized deterministically.

Rules:

```text
UTF-8 encoding
sorted object keys
no insignificant whitespace
arrays preserve order
numbers represented as strings in v0
no NaN or Infinity
no timestamps inside semantic objects
no host-specific paths inside semantic objects
```

Timestamps, agent IDs, comments, and explanations belong in migrations or metadata, not semantic objects.

## 9. Required CLI

The v0 CLI should expose these commands:

```text
codedb init <db>
codedb import <db> <file>
codedb export <db> --branch main --out <file>
codedb run <db> <function-name>
codedb list <db>
codedb show <db> <symbol-or-name>
codedb callers <db> <symbol-or-name>
codedb rename <db> <old-name> <new-name>
codedb replace-body <db> <name> <expr>
codedb diff <db> <root-a> <root-b>
codedb history <db>
codedb replay <db> --from-genesis
codedb verify <db>
```

### 9.1 Demo command sequence

The first demo should work like this:

```bash
codedb init demo.codedb.sqlite
codedb import demo.codedb.sqlite examples/shop.cdb
codedb run demo.codedb.sqlite main
codedb callers demo.codedb.sqlite tax
codedb rename demo.codedb.sqlite tax vat
codedb diff demo.codedb.sqlite <old-root> <new-root>
codedb export demo.codedb.sqlite --branch main --out projection.cdb
codedb replay demo.codedb.sqlite --from-genesis
codedb verify demo.codedb.sqlite
```

Expected behavior:

- `run main` returns the numeric result.
- `callers tax` shows `total`.
- `rename tax vat` creates a new root where the symbol hash is unchanged but the preferred name changed.
- `diff` classifies the change as `rename_symbol`, not delete/add.
- `export` renders `vat` everywhere.
- `replay` rebuilds the same final `root_hash`.
- `verify` confirms all object hashes, indexes, dependencies, and history links.

## 10. Migrations

### 10.1 Migration structure

Example migration object:

```json
{
  "schema_version": 1,
  "input_root_hash": "sha256:old-root",
  "operation": {
    "kind": "rename_symbol",
    "symbol": "sha256:symbol-hash",
    "old_name": "tax",
    "new_name": "vat",
    "module": "main"
  },
  "preconditions": [
    {
      "kind": "root_is_current",
      "root": "sha256:old-root"
    },
    {
      "kind": "name_points_to_symbol",
      "module": "main",
      "name": "tax",
      "symbol": "sha256:symbol-hash"
    },
    {
      "kind": "name_is_available",
      "module": "main",
      "name": "vat"
    }
  ],
  "postconditions": [
    {
      "kind": "name_points_to_symbol",
      "module": "main",
      "name": "vat",
      "symbol": "sha256:symbol-hash"
    },
    {
      "kind": "name_absent",
      "module": "main",
      "name": "tax"
    }
  ]
}
```

### 10.2 Idempotence rule

Every migration application must return one of three outcomes:

```text
applied
already_applied
conflict
```

Rules:

```text
If preconditions hold:
  apply operation and verify postconditions.

If preconditions do not hold but postconditions already hold:
  return already_applied.

If neither preconditions nor postconditions hold:
  return conflict.
```

This makes migrations safe to retry.

### 10.3 Required v0 migrations

#### create_function

Creates:

- a new `Symbol`,
- a new `FunctionDef`,
- name metadata,
- a new `ProgramRoot`.

Required operation fields:

```json
{
  "kind": "create_function",
  "module": "main",
  "name": "tax",
  "birth_seed": "deterministic-or-agent-provided-seed",
  "params": ["subtotal"],
  "body_expr": {
    "kind": "binary",
    "op": "*",
    "left": { "kind": "param_ref", "index": 0 },
    "right": { "kind": "literal_number", "value": "0.20" }
  }
}
```

#### rename_symbol

Changes name metadata only.

Must not change:

- symbol hash,
- definition hash,
- expression hashes,
- dependency graph.

#### replace_function_body

Creates:

- new expression objects as needed,
- a new `FunctionDef`,
- a new `ProgramRoot`,
- refreshed dependency indexes.

Must not change:

- symbol hash,
- preferred name unless explicitly requested.

#### delete_symbol

Removes a symbol from the root only if no live definitions depend on it, unless forced.

#### create_alias

Adds an additional name for an existing symbol.

## 11. Diff model

The diff engine compares two root hashes.

### 11.1 Hash-pruned traversal

Algorithm:

```text
if hash_a == hash_b:
  return unchanged
else:
  compare object kinds
  compare payload fields
  recursively compare child hashes
```

Equal hashes prove equal subgraphs and should not be traversed.

### 11.2 Semantic classification

The diff engine should classify changes at the highest semantic level possible.

Required v0 classifications:

```text
symbol_added
symbol_removed
symbol_renamed
alias_added
alias_removed
function_body_changed
function_signature_changed
dependency_added
dependency_removed
literal_changed
call_target_changed
expression_replaced
```

Examples:

Same symbol hash, different preferred name:

```text
symbol_renamed
```

Same symbol hash, different definition hash:

```text
function_body_changed
```

Different body hashes but same interface hash:

```text
implementation_changed
```

Different interface hash:

```text
interface_changed
```

### 11.3 Human-readable diff projection

Example output:

```text
Root changed:
  from sha256:old-root
  to   sha256:new-root

Renamed symbol:
  symbol: sha256:symbol-hash
  main.tax -> main.vat

Unchanged:
  function body hash
  dependencies
  callers
```

For a body change:

```text
Changed function: main.total
  symbol: sha256:...

Expression diff:
  literal changed:
    0.20 -> 0.18

Dependency diff:
  no dependency changes

Incremental compile impact:
  recompile main.total
  callers do not require recompilation if interface hash unchanged
```

## 12. Interpreter

The interpreter runs from a root hash.

Inputs:

```text
root_hash
entry symbol or entry name
argument values
```

Evaluation rules:

```text
Resolve entry name to symbol hash in root_names.
Resolve symbol hash to current FunctionDef in root_symbols.
Evaluate function body expression.
Calls resolve target symbol through the same root.
```

This means the same symbol call can evaluate differently under different roots if the target symbol is rebound to a different definition in those roots.

The interpreter must not read source files.

## 13. Incremental compilation and caching

v0 does not need native compilation, but it should include the cache model.

### 13.1 Cache key

A cache key must include all inputs that affect an artifact.

Example:

```text
cache_key = sha256(
  "codedb/cache/v1\0" ||
  input_hash ||
  dependency_interface_hash ||
  backend ||
  target ||
  compiler_version ||
  runtime_version ||
  pipeline_version
)
```

### 13.2 Required v0 cached artifacts

Required:

```text
rendered_source
parsed_expression
function_dependency_set
interface_hash
implementation_hash
```

Optional:

```text
typed_expression
normalized_ir
bytecode
llvm_ir
```

### 13.3 Invalidation

If an implementation hash changes but interface hash does not:

```text
recompute the changed function artifact
recompute whole-program run cache if any
callers may not need recompilation
```

If interface hash changes:

```text
recompute direct dependents
then recursively recompute affected dependents
```

## 14. Projections

A projection is a readable rendering of a root.

v0 only needs one projection:

```text
canonical_source
```

Example:

```text
def vat(subtotal) = subtotal * 0.20

def total(subtotal) = subtotal + vat(subtotal)

def main() = total(100)
```

Projection rules:

```text
Use preferred names from root metadata.
Render definitions in deterministic order.
Use stable formatting.
Do not preserve original whitespace.
Do not preserve comments in v0.
Emit enough information to re-import the projection if possible.
```

Post-v0 projections:

```text
one_file_per_function
public_api_only
dependency_order
callers_of_symbol
tests_only
literate_markdown
llvm_ir
```

## 15. Import model

Import from text is allowed for bootstrapping, but agents should eventually use structural migrations directly.

Import should parse a canonical source file and produce a sequence of migrations.

For v0, import can be implemented as:

```text
empty root
for each function in file:
  create_function migration
```

A later importer can diff text against an existing root and synthesize migrations.

## 16. Verification

`codedb verify` must check:

```text
all object hashes match payloads
all object_edges point to existing objects
all branch roots exist
all migration input/output roots exist
all history hashes are correct
materialized root_symbols can be rebuilt from root objects
materialized root_names can be rebuilt from root objects
materialized dependencies can be rebuilt from expression DAGs
history replay produces the branch root hash
no cache entries claim impossible inputs
```

Verification failure should distinguish:

```text
corrupt_object
missing_object
bad_hash
bad_history_link
bad_index
bad_dependency_index
bad_cache_entry
```

## 17. Rebuild from scratch

A fresh database can be rebuilt from:

```text
genesis root
migration log
```

Replay process:

```text
current_root = genesis_root
current_history = null

for migration in migration_order:
  assert migration.input_root_hash == current_root
  outcome = apply_migration(current_root, migration)
  assert outcome in [applied, already_applied]
  assert produced_root == migration.output_root_hash
  current_root = produced_root
  current_history = hash_history(current_history, migration.hash, produced_root)

assert current_root == expected_branch_root
assert current_history == expected_history_head
```

If migrations are exported as newline-delimited JSON, the system can rebuild without copying the original SQLite file.

## 18. Branches and merge

v0 supports branches only as named pointers:

```text
branch name -> root_hash + history_hash
```

No merge algorithm is required for v0.

Post-v0 merge should use:

```text
common ancestor root
migration replay
semantic conflict detection
hash-pruned tree diff
```

## 19. Agent write API

Agents should primarily modify the program through structural operations.

Required v0 operation API:

```text
create_function(module, name, params, body_ast)
rename_symbol(symbol_or_name, new_name)
replace_function_body(symbol_or_name, body_ast)
delete_symbol(symbol_or_name)
create_alias(symbol_or_name, alias)
```

Each API call must produce a migration.

The API must return:

```text
old_root_hash
new_root_hash
migration_hash
history_hash
semantic_summary
```

Example response:

```json
{
  "status": "applied",
  "old_root_hash": "sha256:old-root",
  "new_root_hash": "sha256:new-root",
  "migration_hash": "sha256:migration",
  "history_hash": "sha256:history",
  "summary": {
    "kind": "rename_symbol",
    "symbol": "sha256:symbol-hash",
    "from": "main.tax",
    "to": "main.vat"
  }
}
```

## 20. First demo program

File: `examples/shop.cdb`

```text
def tax(subtotal) = subtotal * 0.20

def total(subtotal) = subtotal + tax(subtotal)

def main() = total(100)
```

Demo assertions:

```text
main() evaluates to 120
callers(tax) returns total
rename tax -> vat preserves symbol hash
rename tax -> vat changes root hash
rename tax -> vat does not change function body hash
export renders vat in total body
replay produces identical root hash
verify passes
```

## 21. Suggested implementation milestones

### Milestone 1 — Object store

Deliver:

```text
canonical_json
hash_object
insert_object
get_object
object_edges
verify_object_hashes
```

Acceptance:

```text
same payload always gets same hash
different semantic payloads get different hashes
objects are immutable
verify catches tampered payload_json
```

### Milestone 2 — Tiny AST and interpreter

Deliver:

```text
expression objects
function definition objects
program root object
root_symbols index
root_names index
run command
```

Acceptance:

```text
shop.cdb imports
main evaluates to 120
interpreter reads from SQLite, not source file
```

### Milestone 3 — Migrations

Deliver:

```text
create_function
rename_symbol
replace_function_body
migration table
history table
branch update
idempotent apply
```

Acceptance:

```text
rename is retry-safe
replace-body creates new root
history chain verifies
```

### Milestone 4 — Projection

Deliver:

```text
canonical source export
name rendering
function ordering
```

Acceptance:

```text
export after rename shows new name everywhere
export is deterministic
```

### Milestone 5 — Semantic diff

Deliver:

```text
root diff
symbol rename classification
function body change classification
expression diff
compile-impact summary
```

Acceptance:

```text
rename is not shown as delete/add
literal change is shown as literal change
unchanged subtrees are skipped by hash
```

### Milestone 6 — Replay and verify

Deliver:

```text
replay from genesis
verify indexes
verify dependencies
verify history
```

Acceptance:

```text
replay produces the same root hash
verify passes on clean DB
verify fails on intentionally corrupted DB
```

## 22. Post-v0 roadmap

After the v0 demo works, likely next steps are:

```text
richer expression language
structural agent API over HTTP or MCP
local variable identity
type system
interface hash / implementation hash distinction
branch merge
migration squashing
semantic patch language
bytecode backend
MLIR or LLVM backend
Go/Rust/C projection backend
visual graph explorer
```

## 23. Design principles

1. Files are projections.
2. Source truth is the object DAG.
3. The current state is identified by `root_hash`.
4. The explanation of the current state is `migration_history`.
5. Names are metadata.
6. Global references use symbol hashes, not names.
7. Semantic changes are migrations, not line edits.
8. Diffs should preserve intent when possible.
9. Equal hashes prove equal subgraphs.
10. Rebuild must be deterministic.
11. Caches are disposable.
12. Verification is mandatory.

## 24. Open questions

These do not block v0, but should be revisited:

1. Should v1 use BLAKE3 instead of SHA-256?
2. Should migration logs be stored inside SQLite only, or also exported as NDJSON?
3. Should symbol birth seeds be random, agent-provided, or deterministically derived from the creating migration?
4. Should root metadata changes count as semantic root changes, or should there be separate semantic and presentation roots?
5. Should function calls bind to symbol hashes or to exact definition hashes?
6. How should branch merge conflicts be represented?
7. Should import from text be allowed to synthesize migrations against a non-empty root?
8. How should comments/docstrings be modeled: metadata, semantic documentation objects, or projection-only text?
9. Should the first native backend target LLVM IR, MLIR, WebAssembly, C, or Go?
10. Should the object store eventually support garbage collection of unreachable objects?
