# SPEC.md — CodeDB Proof of Concept

Status: Draft 0.2  
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
- The target language is statically typed and does not require a managed runtime.

## 2. Core design correction from Draft 0.1

Draft 0.1 recommended Python and a tree-walking interpreter because the first goal was to prove storage, migration, and diff mechanics quickly.

Draft 0.2 changes the recommendation:

```text
Implementation language: Rust
Target language:        statically typed, runtime-free core
Primary execution:      reference evaluator + no-runtime compiled artifact
Parser:                 optional; structural agent API is primary
```

Rationale:

- The implementation itself benefits from Rust's strong types, enums, pattern matching, ownership model, and lack of garbage collection.
- The target language's semantics should be modeled as typed objects from the beginning.
- Agents are the expected authors, so the primary authoring API can be structural instead of text-first.
- A reference evaluator may exist for testing, but generated programs should not depend on a VM, garbage collector, scheduler, exception system, or reflection runtime.

## 3. First implementation goal

Build a minimal database-backed, statically typed language implementation that can:

1. Store a tiny program as a content-addressed DAG in SQLite.
2. Represent the current program state as a `ProgramRoot` object hash.
3. Apply idempotent semantic migrations that transform one root into another.
4. Rebuild database state from genesis plus migration history.
5. Type-check definitions and expressions from the semantic DAG.
6. Generate readable source projections from the current root.
7. Produce semantic diffs between two roots.
8. Evaluate a tiny program through a reference evaluator for tests.
9. Emit a small no-runtime compiled artifact for the typed core language.
10. Cache typed/lowered/compiled artifacts by content hash.

The first implementation should prove the storage, migration, type, diff, and no-runtime compilation model, not language richness.

## 4. Non-goals for v0

The first implementation does not attempt to provide:

- A production language.
- A hand-written developer editing experience.
- A full LSP.
- A custom projectional editor.
- A package manager.
- Git-style merge support.
- Perfect textual round-tripping.
- Comment/whitespace preservation.
- Heap allocation.
- Garbage collection.
- Exceptions.
- Async/green-thread scheduling.
- Dynamic reflection.
- Dynamic dispatch.
- A production optimizer.
- A production native-code backend.

A minimal C or LLVM IR emission path is allowed in v0 only to demonstrate that the language core does not need a managed runtime.

## 5. Implementation stack

Recommended v0 stack:

```text
Language:       Rust
Database:       SQLite
SQLite access:  rusqlite or equivalent
Hashing:        SHA-256 for v0, BLAKE3 later if desired
Serialization:  canonical JSON or canonical binary encoding
CLI:            clap or equivalent
Parser:         optional; structural API first
Evaluator:      small reference evaluator for tests only
Backend:        C projection or LLVM IR text for no-runtime artifact
```

Rust is recommended for Draft 0.2 because the hard questions are now not only data modeling, hashing, migrations, diffs, and invalidation, but also static typing, typed IR invariants, and no-runtime compilation.

Python remains acceptable for a spike, but not for the main implementation if the goal is to build a real compiler-like system.

Go remains acceptable for tooling, but generating Go as the target output is not aligned with the target language's no-runtime goal.

## 6. Meaning of "no runtime"

For this project, "no runtime" means generated programs do not require a language-managed runtime such as:

```text
VM
bytecode interpreter
garbage collector
green-thread scheduler
exception unwinder as a language feature
reflection system
implicit allocator
large standard runtime library
```

This does not mean the compiler implementation cannot use libraries.

This also does not mean that an operating-system executable has literally zero startup or ABI support. For v0, the compiled artifact can be an object file or C/LLVM function that is linked into a tiny external harness for testing.

The generated language core should avoid:

```text
implicit heap allocation
implicit panics
exceptions
runtime reflection
closures requiring allocation
dynamic trait/interface dispatch
runtime type metadata
```

Allowed in v0:

```text
integer arithmetic
boolean operations
pure function calls
conditionals
static dispatch
explicit machine-level traps if required
```

## 7. Core concepts

### 7.1 Object

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
  "codedb/object/v1\0" || kind || "\0" || schema_version || "\0" || canonical_payload
)
```

The exact hash algorithm may change later, but v0 must use one deterministic algorithm everywhere.

### 7.2 Program root

A `ProgramRoot` object represents a complete program state.

It points to:

- the current symbol-to-definition map,
- name metadata,
- module metadata,
- type metadata,
- optional projection metadata.

Changing a definition, name, module assignment, type binding, or metadata object produces a new `ProgramRoot` hash.

### 7.3 Migration history

A migration is a semantic operation that transforms one root into another.

Examples:

```text
create_function
rename_symbol
replace_function_body
change_function_signature
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

### 7.4 Symbol identity

Global symbol names are metadata.

A symbol's stable identity is the content hash of an immutable `SymbolBirth` object. This object must not include the current display name, function body, function signature, or module path.

Example `SymbolBirth` payload:

```json
{
  "symbol_kind": "function",
  "birth_history_hash": "sha256:...",
  "local_nonce": "00000001"
}
```

This gives separate identities:

```text
symbol_hash          stable identity of the symbol
name                 display metadata for humans/projections
function_sig_hash    current callable interface
function_def_hash    current implementation bound to that symbol in a root
body_expr_hash       current implementation body
```

Important rule:

```text
Renaming a symbol must not change its symbol_hash.
Changing a function body must not change its symbol_hash.
Changing a function signature must not change its symbol_hash, but must change its function_sig_hash.
```

### 7.5 Type identity

Types are content-addressed objects.

Required v0 types:

```text
I64
Bool
Unit
FunctionSignature
```

Optional post-v0 types:

```text
U64
I32
F64
Pointer
Record
Enum
Array
Slice
Result
```

A function signature object contains:

```text
parameter type hashes
return type hash
calling convention / ABI tag
effect information, if any
```

Example `FunctionSignature` payload:

```json
{
  "params": ["sha256:type-i64"],
  "return": "sha256:type-i64",
  "abi": "codedb-v0-internal",
  "effects": []
}
```

### 7.6 Definition identity

A function definition is content-addressed.

A function definition hash changes when its semantic implementation or signature binding changes.

A `FunctionDef` object contains:

```text
symbol_hash
function_sig_hash
typed_body_expr_hash
```

Display names for the function and parameters belong in metadata, not in the semantic definition.

### 7.7 Expressions form a DAG

Expressions are immutable content-addressed objects.

They may reference other expressions by hash. Therefore, the internal representation is a DAG, not necessarily a tree.

This allows:

- expression reuse,
- shared subexpressions,
- cheap equality checks,
- hash-pruned diffs,
- incremental compilation,
- cached evaluation/lowering.

## 8. v0 language

The v0 language is deliberately tiny, statically typed, pure, and runtime-free.

### 8.1 Projection syntax

Example projection:

```text
fn tax(subtotal: i64) -> i64 = subtotal * 20 / 100

fn total(subtotal: i64) -> i64 = subtotal + tax(subtotal)

fn main() -> i64 = total(100)
```

Notes:

- Use integer arithmetic in v0.
- Avoid floating-point, strings, heap objects, and allocation in the first demo.
- Parameter names are projection metadata.
- Calls bind to symbol hashes after resolution.

### 8.2 Expression forms

Required v0 expression kinds:

```text
literal_i64
literal_bool
param_ref
call
binary
if
```

Each expression has a statically known type.

Optional after the basic demo:

```text
let
unit
record
field_access
fixed_array
```

### 8.3 Expression object examples

Integer literal:

```json
{
  "kind": "literal_i64",
  "value": "100",
  "type": "sha256:type-i64"
}
```

Parameter reference:

```json
{
  "kind": "param_ref",
  "index": 0,
  "type": "sha256:type-i64"
}
```

Function call:

```json
{
  "kind": "call",
  "symbol": "sha256:symbol-hash",
  "args": ["sha256:expr-arg"],
  "type": "sha256:type-i64"
}
```

Binary expression:

```json
{
  "kind": "binary",
  "op": "+",
  "left": "sha256:expr-left",
  "right": "sha256:expr-right",
  "type": "sha256:type-i64"
}
```

Conditional:

```json
{
  "kind": "if",
  "cond": "sha256:expr-cond",
  "then": "sha256:expr-then",
  "else": "sha256:expr-else",
  "type": "sha256:type-i64"
}
```

## 9. Type checking

Type checking is mandatory in v0.

Inputs:

```text
root_hash
symbol_hash
candidate expression DAG
function signature
```

Required checks:

```text
literal_i64 has type I64
literal_bool has type Bool
param_ref index is in bounds and has the declared parameter type
binary integer ops require I64 operands and return I64
comparison ops require I64 operands and return Bool
if condition must be Bool
if branches must have the same type
call arguments must match callee signature
function body type must match function return type
```

The type checker produces or verifies typed expression objects.

A failed type check must prevent creation of a new root unless the migration is explicitly marked as an invalid/staged edit. v0 does not need staged invalid edits.

## 10. SQLite schema

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
    signature_hash TEXT NOT NULL,
    PRIMARY KEY (root_hash, symbol_hash),
    FOREIGN KEY (root_hash) REFERENCES objects(hash) ON DELETE CASCADE,
    FOREIGN KEY (symbol_hash) REFERENCES objects(hash),
    FOREIGN KEY (definition_hash) REFERENCES objects(hash),
    FOREIGN KEY (signature_hash) REFERENCES objects(hash)
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

## 11. Canonical encoding

All semantic object payloads must be serialized deterministically.

Rules for v0 canonical JSON:

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

A canonical binary encoding may replace canonical JSON later, but the hash rules must remain deterministic and versioned.

## 12. Required CLI

The v0 CLI should expose these commands:

```text
codedb init <db>
codedb import <db> <file>
codedb export <db> --branch main --out <file>
codedb eval <db> <function-name>
codedb emit-c <db> <function-name> --out <file>
codedb emit-llvm <db> <function-name> --out <file>        # optional in v0
codedb list <db>
codedb show <db> <symbol-or-name>
codedb callers <db> <symbol-or-name>
codedb rename <db> <old-name> <new-name>
codedb replace-body <db> <name> <expr>
codedb change-signature <db> <name> <signature>
codedb diff <db> <root-a> <root-b>
codedb history <db>
codedb replay <db> --from-genesis
codedb verify <db>
```

### 12.1 Demo command sequence

The first demo should work like this:

```bash
codedb init demo.codedb.sqlite
codedb import demo.codedb.sqlite examples/shop.cdb
codedb eval demo.codedb.sqlite main
codedb callers demo.codedb.sqlite tax
codedb rename demo.codedb.sqlite tax vat
codedb diff demo.codedb.sqlite <old-root> <new-root>
codedb export demo.codedb.sqlite --branch main --out projection.cdb
codedb emit-c demo.codedb.sqlite main --out projection.c
codedb replay demo.codedb.sqlite --from-genesis
codedb verify demo.codedb.sqlite
```

Expected behavior:

- `eval main` returns `120`.
- `callers tax` shows `total`.
- `rename tax vat` creates a new root where the symbol hash is unchanged but the preferred name changed.
- `diff` classifies the change as `rename_symbol`, not delete/add.
- `export` renders `vat` everywhere.
- `emit-c` emits a pure, allocation-free C projection of the current root.
- `replay` rebuilds the same final `root_hash`.
- `verify` confirms all object hashes, indexes, dependencies, types, and history links.

## 13. Migrations

### 13.1 Migration structure

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

### 13.2 Idempotence rule

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

### 13.3 Required v0 migrations

#### create_function

Creates:

- a new `SymbolBirth`,
- a new `FunctionSignature`,
- typed expression objects,
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
  "params": [
    { "name": "subtotal", "type": "i64" }
  ],
  "return_type": "i64",
  "body_expr": {
    "kind": "binary",
    "op": "/",
    "left": {
      "kind": "binary",
      "op": "*",
      "left": { "kind": "param_ref", "index": 0 },
      "right": { "kind": "literal_i64", "value": "20" }
    },
    "right": { "kind": "literal_i64", "value": "100" }
  }
}
```

#### rename_symbol

Changes name metadata only.

Must not change:

- symbol hash,
- signature hash,
- definition hash,
- expression hashes,
- dependency graph.

#### replace_function_body

Creates:

- new expression objects as needed,
- a new typed expression DAG,
- a new `FunctionDef`,
- a new `ProgramRoot`,
- refreshed dependency indexes.

Must verify:

- new body type matches the current function signature return type,
- all calls match callee signatures,
- all expression types are valid.

Must not change:

- symbol hash,
- preferred name unless explicitly requested,
- function signature unless explicitly requested.

#### change_function_signature

Creates:

- a new `FunctionSignature`,
- a new `FunctionDef`,
- a new `ProgramRoot`,
- refreshed dependency indexes.

Must either:

- update all call sites in the same migration, or
- fail if existing calls become invalid.

#### delete_symbol

Removes a symbol from the root only if no live definitions depend on it, unless forced.

#### create_alias

Adds an additional name for an existing symbol.

## 14. Diff model

The diff engine compares two root hashes.

### 14.1 Hash-pruned traversal

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

### 14.2 Semantic classification

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
type_changed
interface_changed
implementation_changed
```

Examples:

Same symbol hash, different preferred name:

```text
symbol_renamed
```

Same symbol hash, different definition hash, same signature hash:

```text
implementation_changed
```

Same symbol hash, different signature hash:

```text
interface_changed
```

### 14.3 Human-readable diff projection

Example output:

```text
Root changed:
  from sha256:old-root
  to   sha256:new-root

Renamed symbol:
  symbol: sha256:symbol-hash
  main.tax -> main.vat

Unchanged:
  signature hash
  function body hash
  dependencies
  callers
  compiled artifact cache key for implementation
```

For a body change:

```text
Changed function: main.total
  symbol: sha256:...
  signature: unchanged

Expression diff:
  literal changed:
    20 -> 18

Dependency diff:
  no dependency changes

Incremental compile impact:
  recompile main.total
  callers do not require recompilation because interface hash is unchanged
```

## 15. Reference evaluator and backend

### 15.1 Reference evaluator

The reference evaluator runs from a root hash and exists for testing and debugging.

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
Verify or load typed body expression.
Evaluate function body expression.
Calls resolve target symbol through the same root.
```

The evaluator must not read source files.

The evaluator is not the target language runtime.

### 15.2 No-runtime backend

The v0 backend should emit a tiny no-runtime artifact.

Acceptable v0 backend choices:

```text
C source projection with no allocation and no library calls
LLVM IR text with no external runtime calls
object file exposing a plain ABI function
```

Recommended first backend:

```text
emit C for pure i64/bool functions
compile or inspect separately
```

Example generated C projection:

```c
long codedb_tax(long subtotal) {
    return (subtotal * 20) / 100;
}

long codedb_total(long subtotal) {
    return subtotal + codedb_tax(subtotal);
}

long codedb_main(void) {
    return codedb_total(100);
}
```

This generated code should not use:

```text
malloc
free
printf
exceptions
threads
global runtime initialization
reflection metadata
```

A separate test harness may call `codedb_main` and print the result. The harness is not part of the target language runtime.

## 16. Incremental compilation and caching

v0 should include the cache model.

### 16.1 Cache key

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

For no-runtime output, `runtime_version` should be a sentinel such as:

```text
runtime:none
```

### 16.2 Required v0 cached artifacts

Required:

```text
rendered_source
parsed_expression
typed_expression
function_dependency_set
interface_hash
implementation_hash
c_projection
```

Optional:

```text
normalized_ir
llvm_ir
object_file
```

### 16.3 Invalidation

If an implementation hash changes but interface hash does not:

```text
recompute the changed function artifact
recompute whole-program artifact if needed
callers may not need recompilation
```

If interface hash changes:

```text
recompute direct dependents
then recursively recompute affected dependents
```

## 17. Projections

A projection is a readable rendering of a root.

v0 needs two projections:

```text
canonical_source
c_backend_source
```

Example canonical source:

```text
fn vat(subtotal: i64) -> i64 = subtotal * 20 / 100

fn total(subtotal: i64) -> i64 = subtotal + vat(subtotal)

fn main() -> i64 = total(100)
```

Projection rules:

```text
Use preferred names from root metadata.
Render definitions in deterministic order.
Use stable formatting.
Include type annotations.
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

## 18. Import model

Import from text is allowed for bootstrapping, but agents should eventually use structural migrations directly.

Import should parse a canonical source file and produce a sequence of migrations.

For v0, import can be implemented as:

```text
empty root
for each function in file:
  create_function migration with signature and body
  type-check before committing root
```

A later importer can diff text against an existing root and synthesize migrations.

## 19. Verification

`codedb verify` must check:

```text
all object hashes match payloads
all object_edges point to existing objects
all branch roots exist
all migration input/output roots exist
all history hashes are correct
all function signatures are valid
all typed expressions are type-correct
all function bodies match their declared return types
materialized root_symbols can be rebuilt from root objects
materialized root_names can be rebuilt from root objects
materialized dependencies can be rebuilt from expression DAGs
history replay produces the branch root hash
no cache entries claim impossible inputs
no no-runtime backend artifact contains forbidden runtime calls, where checkable
```

Verification failure should distinguish:

```text
corrupt_object
missing_object
bad_hash
bad_history_link
bad_index
bad_dependency_index
bad_type
bad_signature
bad_cache_entry
forbidden_runtime_dependency
```

## 20. Rebuild from scratch

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

## 21. Branches and merge

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

## 22. Agent write API

Agents should primarily modify the program through structural operations.

Required v0 operation API:

```text
create_function(module, name, params_with_types, return_type, body_ast)
rename_symbol(symbol_or_name, new_name)
replace_function_body(symbol_or_name, body_ast)
change_function_signature(symbol_or_name, new_signature, callsite_updates)
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
typecheck_summary
compile_impact_summary
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
    "to": "main.vat",
    "typecheck": "unchanged",
    "compile_impact": "metadata_only"
  }
}
```

## 23. First demo program

File: `examples/shop.cdb`

```text
fn tax(subtotal: i64) -> i64 = subtotal * 20 / 100

fn total(subtotal: i64) -> i64 = subtotal + tax(subtotal)

fn main() -> i64 = total(100)
```

Demo assertions:

```text
main() evaluates to 120
callers(tax) returns total
rename tax -> vat preserves symbol hash
rename tax -> vat changes root hash
rename tax -> vat does not change function signature hash
rename tax -> vat does not change function body hash
export renders vat in total body
emit-c produces allocation-free C code
replay produces identical root hash
verify passes
```

## 24. Suggested implementation milestones

### Milestone 1 — Rust project skeleton and object store

Deliver:

```text
Rust workspace
SQLite schema setup
canonical_json or canonical encoding
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

### Milestone 2 — Types and typed AST objects

Deliver:

```text
Type objects
FunctionSignature objects
Expression objects with type fields
FunctionDef objects
ProgramRoot object
root_symbols index
root_names index
```

Acceptance:

```text
type hashes are deterministic
function signatures hash deterministically
function definitions reference typed body expressions
```

### Milestone 3 — Type checker and reference evaluator

Deliver:

```text
type_check_expr
type_check_function
reference evaluator
eval command
```

Acceptance:

```text
shop.cdb imports
main evaluates to 120
type errors prevent root creation
reference evaluator reads from SQLite, not source file
```

### Milestone 4 — Migrations

Deliver:

```text
create_function
rename_symbol
replace_function_body
change_function_signature
migration table
history table
branch update
idempotent apply
```

Acceptance:

```text
rename is retry-safe
replace-body creates new typed root
invalid replacement body is rejected
history chain verifies
```

### Milestone 5 — No-runtime C backend

Deliver:

```text
emit-c command
C projection for i64/bool pure functions
backend cache entries
forbidden runtime call check where practical
```

Acceptance:

```text
emit-c produces deterministic C
emitted C contains no malloc/free/printf/thread calls
codedb_main returns 120 when called by an external harness
```

### Milestone 6 — Projection

Deliver:

```text
canonical source export
name rendering
function ordering
type annotation rendering
```

Acceptance:

```text
export after rename shows new name everywhere
export is deterministic
export includes type annotations
```

### Milestone 7 — Semantic diff

Deliver:

```text
root diff
symbol rename classification
function body change classification
function signature change classification
expression diff
compile-impact summary
```

Acceptance:

```text
rename is not shown as delete/add
literal change is shown as literal change
signature change is shown as interface change
unchanged subgraphs are skipped by hash
```

### Milestone 8 — Replay and verify

Deliver:

```text
replay from genesis
verify indexes
verify dependencies
verify types
verify history
```

Acceptance:

```text
replay produces the same root hash
verify passes on clean DB
verify fails on intentionally corrupted DB
```

## 25. Post-v0 roadmap

After the v0 demo works, likely next steps are:

```text
richer expression language
structural agent API over HTTP or MCP
local variable identity
records and enums
ownership / borrowing / region model
explicit allocation model
interface hash / implementation hash distinction refinements
branch merge
migration squashing
semantic patch language
LLVM IR backend
MLIR backend
Wasm backend
visual graph explorer
```

## 26. Design principles

1. Files are projections.
2. Source truth is the object DAG.
3. The current state is identified by `root_hash`.
4. The explanation of the current state is `migration_history`.
5. Names are metadata.
6. Global references use symbol hashes, not names.
7. Symbol hashes must be stable across renames, body changes, and signature changes.
8. Function signatures are typed, content-addressed interface objects.
9. Semantic changes are migrations, not line edits.
10. Diffs should preserve intent when possible.
11. Equal hashes prove equal subgraphs.
12. The target language core is statically typed.
13. Generated programs do not require a managed runtime.
14. Rebuild must be deterministic.
15. Caches are disposable.
16. Verification is mandatory.

## 27. Open questions

These do not block v0, but should be revisited:

1. Should v1 use BLAKE3 instead of SHA-256?
2. Should migration logs be stored inside SQLite only, or also exported as NDJSON?
3. Should symbol birth seeds be random, agent-provided, or deterministically derived from the creating migration?
4. Should root metadata changes count as semantic root changes, or should there be separate semantic and presentation roots?
5. Should function calls bind to symbol hashes or exact definition hashes?
6. How should branch merge conflicts be represented?
7. Should import from text be allowed to synthesize migrations against a non-empty root?
8. How should comments/docstrings be modeled: metadata, semantic documentation objects, or projection-only text?
9. Should the first native backend target C, LLVM IR, MLIR, WebAssembly, or object code directly?
10. Should the object store eventually support garbage collection of unreachable objects?
11. What ownership/borrowing/allocation model should exist post-v0?
12. Should integer overflow be defined, trapped, wrapping, or statically prevented?
13. Should bounds checks be required, optional, or statically proven for future arrays/slices?
