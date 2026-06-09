# SPEC.md — CodeDB v0 Proof of Concept

Status: Draft 0.4
Scope: v0 Rust proof of concept plus the implemented native-artifact architecture.
The v1 semantic workspace design track lives in [SPEC_V1.md](SPEC_V1.md).
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
- `migration_history_head` identifies the ordered and replayable history that produced that state.
- SQLite stores the object DAG, migration log, indexes, and disposable caches.
- Agents write code through structural operations, not primarily through text files.
- The target language core is statically typed and does not require a managed runtime.
- Compilation artifacts are derived from typed semantic objects, not from source files.

## 2. Core design correction from Draft 0.2

Draft 0.2 allowed a C or LLVM IR emission path as the no-runtime artifact. That was useful for the first proof of concept, but the wording left a dangerous ambiguity: it made the C emitter sound like the compiler backend.

Draft 0.3 makes the backend boundary explicit:

```text
Primary compiler path:
  typed semantic DAG
    -> lowered IR
    -> per-function / per-codegen-unit object artifacts
    -> link plan
    -> executable / ELF / shared object

Projection path:
  ProgramRoot
    -> canonical source projection
    -> C source projection
```

The C output is useful for inspection, testing, debugging, bootstrap demonstrations, and proving that the language core does not require a managed runtime. It is not the long-term compiler architecture.

The compiler architecture should be object-artifact first:

```text
FunctionDef + dependency interfaces + target options
  -> object_file bytes

object_file hashes + export map + entry symbol + link options
  -> LinkPlan

LinkPlan
  -> executable / ELF / Mach-O / shared object
```

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
9. Emit a C source projection for no-runtime inspection.
10. Produce native object artifacts from typed/lowered functions.
11. Link object artifacts through explicit link plans.
12. Cache typed, lowered, object, and link artifacts by complete content-derived keys.

The first implementation should prove the storage, migration, type, diff, incremental compilation, and no-runtime artifact model. It should not prioritize language richness.

## 4. Non-goals for v0

The first implementation does not attempt to provide:

- A production language.
- A hand-written developer editing experience.
- A full LSP.
- A custom projectional editor.
- A package manager.
- Git-style merge support.
- Perfect textual round-tripping.
- Comment or whitespace preservation.
- Heap allocation.
- Garbage collection.
- Exceptions.
- Async or green-thread scheduling.
- Dynamic reflection.
- Dynamic dispatch.
- A production optimizer.
- A production multi-target native-code backend.

A minimal C projection is allowed in v0 to demonstrate no-runtime semantics. A minimal native object backend is allowed in v0 to prove the real compiler architecture. The native backend may support only a tiny language subset and one target at first.

## 5. Implementation stack

Recommended v0 stack:

```text
Implementation language: Rust
Database:                SQLite
SQLite access:           rusqlite or equivalent
Hashing:                 SHA-256 for v0, BLAKE3 later if desired
Serialization:           canonical JSON or canonical binary encoding
CLI:                     clap or equivalent
Parser:                  optional; structural API first
Evaluator:               small reference evaluator for tests only
Projection outputs:      canonical source, C source projection
Compiler backend:        typed DAG -> lowered IR -> object file -> link plan
Initial object targets:  Linux x86-64 ELF relocatable object
                         Apple Silicon arm64 Mach-O relocatable object
```

LLVM, Cranelift, or a custom object writer are implementation choices behind the same artifact interface. The current v0 native path uses small custom object writers for Linux x86-64 ELF and Apple Silicon arm64 Mach-O. The important boundary is that the compiler backend consumes typed/lowered semantic objects and emits object artifacts. It should not depend on a whole-program C file as the primary unit of compilation.

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

This also does not mean that an operating-system executable has literally zero startup or ABI support. For v0, an artifact can be:

```text
relocatable object file
linked executable
shared object
Mach-O object or executable on Apple platforms
C projection linked into an external harness for testing
LLVM/Cranelift-produced object bytes behind the backend interface
```

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
external linker or platform loader support
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
- optional projection metadata,
- optional export metadata.

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
create_alias
remove_alias
set_export
remove_export
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
symbol_hash              stable identity of the symbol
name                     display metadata for humans/projections
function_sig_hash        current callable type-level interface
function_def_hash        current implementation bound to that symbol in a root
body_expr_hash           current implementation body
internal_abi_symbol      stable native symbol name derived from symbol identity
exported_abi_symbol      optional public ABI name from explicit export metadata
```

Important rules:

```text
Renaming a symbol must not change its symbol_hash.
Renaming a symbol must not change its internal native ABI symbol.
Changing a function body must not change its symbol_hash.
Changing a function signature must not change its symbol_hash, but must change its function_sig_hash.
Changing a public exported ABI name is an explicit export-map change, not a normal rename.
```

V0 stores the public export map in `ProgramRoot.exports` as explicit symbol-to-name bindings:

```json
{
  "symbol": "sha256:...",
  "exported_name": "public_tax"
}
```

The internal ABI symbol is derived from stable symbol identity as `codedb_<first 16 hex chars of symbol_hash>`. This name is for native object identity and call relocation. It must not include the preferred display name, aliases, parameter names, or C projection name.

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
- incremental lowering,
- cached type checking,
- cached evaluation,
- cached object-code generation.

## 8. Artifact model

### 8.1 Artifact kinds

Artifacts are derived products. They are disposable and can be regenerated from objects, migrations, and target options.

Required artifact kinds:

```text
canonical_source
c_projection
typed_expression
function_dependency_set
interface_hash
implementation_hash
lowered_ir
object_file
link_plan
executable
```

`c_projection` is a projection artifact. `object_file`, `link_plan`, and `executable` are compiler artifacts.

The current implementation's concrete object payloads, cache metadata wrappers,
lowered IR shape, native object metadata, link plan shape, and SQLite table
roles are cataloged in [ARTIFACTS.md](ARTIFACTS.md).

### 8.2 Interface hash

An interface hash describes the callable surface that a caller depends on.

A dependency interface should include:

```text
callee symbol_hash
callee function_sig_hash
callee internal ABI symbol
calling convention / ABI tag
effects relevant to caller codegen
```

Example:

```text
interface_hash = sha256(
  "codedb/interface/v1\0" ||
  symbol_hash || "\0" ||
  function_sig_hash || "\0" ||
  internal_abi_symbol || "\0" ||
  abi_tag || "\0" ||
  effects_hash
)
```

A body-only callee change must not change the callee interface hash.

### 8.3 Implementation hash

An implementation hash describes the function body and all direct codegen-relevant interfaces.

A function implementation hash should include:

```text
symbol_hash
function_def_hash
typed_body_expr_hash
own function_sig_hash
direct dependency interface hashes
semantic lowering version
```

It should not include:

```text
preferred display name
parameter display names
callee implementation hashes
source projection text
```

This lets caller objects survive callee body-only changes.

### 8.4 Lowered function IR

Lowered IR is a compiler-facing representation derived from typed expression DAGs.

For v0, it can be tiny:

```text
function symbol hash
signature hash
parameter slots
literal operations
binary operations
conditional branches or select operations
static calls by symbol hash
return operation
trap operation where semantics require it
```

Lowered IR must not contain display names.

### 8.5 Object artifact

An object artifact is binary output for one function or one deterministic codegen unit.

Object artifact metadata should include:

```text
artifact_kind: object_file
object_format: elf-relocatable / mach-o / coff / wasm-object / ...
target_triple
symbol_hashes included
internal ABI symbols defined
external/internal ABI symbols referenced
relocations
implementation hashes
interface hashes for dependencies
codegen options
artifact bytes hash
```

For v0, one object per function is preferred because it makes incremental behavior obvious.

### 8.6 Link plan

A link plan is a deterministic artifact that explains how object artifacts become a final binary.

A `LinkPlan` contains:

```text
entry symbol hash
entry internal ABI symbol
target triple
object artifact hashes
symbol definitions
symbol references
export map
external symbols
linker options
output kind: executable / shared_object / static_library
```

A link plan should be cached separately from object files.

## 9. v0 language

The v0 language is deliberately tiny, statically typed, pure, and runtime-free.

### 9.1 Projection syntax

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
- Projection names may change on rename; symbol hashes do not.

### 9.2 Expression forms

Required v0 expression kinds:

```text
literal_i64
literal_bool
literal_unit
param_ref
local_ref
call
binary
unary
let
if
```

Each expression has a statically known type.

Optional after the basic demo:

```text
record
field_access
fixed_array
```

### 9.3 Expression object examples

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

Unit literal:

```json
{
  "kind": "literal_unit",
  "type": "sha256:type-unit"
}
```

Unary expression:

```json
{
  "kind": "unary",
  "op": "!",
  "expr": "sha256:expr-bool",
  "type": "sha256:type-bool"
}
```

Let expression with a typed binding:

```json
{
  "kind": "let",
  "binding_name": "x",
  "binding_type": "sha256:type-i64",
  "value": "sha256:expr-value",
  "body": "sha256:expr-body",
  "type": "sha256:type-i64"
}
```

Local reference inside a let body:

```json
{
  "kind": "local_ref",
  "depth": 0,
  "type": "sha256:type-i64"
}
```

## 10. Type checking

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
literal_unit has type Unit
param_ref index is in bounds and has the declared parameter type
local_ref depth is in bounds and has the active let binding type
binary integer ops require I64 operands and return I64
comparison ops require I64 operands and return Bool
boolean ops require Bool operands and return Bool
unary integer negation requires an I64 operand and returns I64
unary boolean not requires a Bool operand and returns Bool
let binding value type must match the declared binding type
let expression type is the type of its body
if condition must be Bool
if branches must have the same type
call arguments must match callee signature
function body type must match function return type
```

The type checker produces or verifies typed expression objects.

A failed type check must prevent creation of a new root unless the migration is explicitly marked as an invalid or staged edit. v0 does not need staged invalid edits.

## 11. SQLite schema

File: `schema.sql`

The current schema can support the v0 compiler model without immediately adding native-specific tables because `compile_cache` already stores structured JSON and optional bytes. Dedicated artifact tables may be added later if cache queries become too complex.

Core tables:

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
    cache_key_json TEXT NOT NULL,
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

Expected cache use:

```text
cache_key_json stores the canonical typed cache-key input.
artifact_kind is the typed artifact discriminator.
backend stores the producer ID; projection artifacts use projection, while compiler artifacts use a backend ID.
artifact_json stores deterministic metadata.
artifact_bytes stores native object or executable bytes.
artifact_hash hashes either canonical artifact_json, artifact_bytes, or both depending on artifact kind.
cache_key hashes every input that affects the artifact.
```

## 12. Canonical encoding

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

## 13. Required CLI

The v0 CLI should expose these commands:

```text
codedb init <db>
codedb import <db> <file>
codedb export <db> --branch main --out <file>
codedb eval <db> <function-name>
codedb emit-c <db> <function-name> --out <file>
codedb list <db>
codedb show <db> <symbol-or-name>
codedb callers <db> <symbol-or-name>
codedb rename <db> <old-name> <new-name> [--expect-root <root>]
codedb replace-body <db> <name> <expr> [--expect-root <root>]
codedb change-signature <db> <name> <signature> [--expect-root <root>]
codedb delete-symbol <db> <name> [--force] [--expect-root <root>]
codedb create-alias <db> <name> <alias> [--expect-root <root>]
codedb diff <db> <root-a> <root-b>
codedb history <db>
codedb replay <db> --from-genesis
codedb verify <db>
```

Near-term compiler commands:

```text
codedb emit-ir <db> <function-name> --out <file>
codedb build-plan <db> <entry-name> --target <triple> --json
codedb emit-object <db> <function-name> --target <triple> --out <file>
codedb link-native <db> <entry-name> --target <triple> --out <file>
codedb build <db> <entry-name> --target <triple> --out <file>
```

Important naming rule:

```text
emit-c emits a C projection.
emit-object emits a compiler artifact.
build/link-native operate on object artifacts and link plans.
```

Structural mutation commands accept `--expect-root` so agents can bind an
operation to the root they inspected. If the branch has moved and the requested
postconditions do not already hold, the command returns `conflict` without
recording a migration or moving the branch pointer.

### 13.1 Demo command sequence

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
- `verify` confirms all object hashes, indexes, dependencies, types, history links, and cache invariants.

Native artifact demo after the backend exists:

```bash
codedb build-plan demo.codedb.sqlite main --target x86_64-unknown-linux-gnu --json
codedb emit-object demo.codedb.sqlite main --target x86_64-unknown-linux-gnu --out main.o
codedb build demo.codedb.sqlite main --target x86_64-unknown-linux-gnu --out demo
```

Expected native behavior:

- Rename does not regenerate object files.
- Body-only change regenerates the changed function object and relinks affected binaries.
- Signature change regenerates the changed function and affected dependents.

## 14. Migrations

### 14.1 Migration structure

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

### 14.2 Idempotence rule

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

### 14.3 Required v0 migrations

For concrete CLI and `codedb/apply/v1` examples for every structural operation,
see [MIGRATIONS.md](MIGRATIONS.md).

#### create_function

Creates:

- a new `SymbolBirth`,
- a new `FunctionSignature`,
- typed expression objects,
- a new `FunctionDef`,
- name metadata,
- optional export metadata,
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
- dependency graph,
- internal ABI symbol,
- native object artifact keys.

The C projection may change because it is a human-readable projection.

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
- function signature unless explicitly requested,
- internal ABI symbol.

Build impact:

```text
recompile changed function object
relink binaries containing that object
callers do not recompile if dependency interface hashes are unchanged
```

#### change_function_signature

Creates:

- a new `FunctionSignature`,
- a new `FunctionDef`,
- a new `ProgramRoot`,
- refreshed dependency indexes.

Must either:

- update all call sites in the same migration, or
- fail if existing calls become invalid.

Build impact:

```text
recompile changed function object
recompute direct and transitive dependent function objects
relink affected binaries
```

#### delete_symbol

Removes a symbol from the root only if no live definitions depend on it, unless forced.

Build impact:

```text
relink binaries that previously included the symbol
recompile dependents only when forced deletion also updates call sites or interfaces
```

#### create_alias

Adds an additional projection name for an existing symbol.

Build impact:

```text
metadata_only
```

#### remove_alias

Removes a non-preferred projection alias for an existing symbol.

Build impact:

```text
metadata_only
```

#### set_export / remove_export

Changes public ABI export metadata.

`set_export(symbol_or_name, exported_name)` adds an explicit public ABI name for a symbol. `remove_export(symbol_or_name, exported_name)` removes that explicit binding. These operations do not rename the symbol and do not change the internal ABI symbol derived from symbol identity.

Build impact depends on representation:

```text
internal ABI unchanged: relink or regenerate export metadata only
internal ABI changed: recompile affected objects and relink
```

The preferred rule is that internal ABI names never change and export-map changes are handled at link time.

## 15. Diff model

The diff engine compares two root hashes.

### 15.1 Hash-pruned traversal

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

### 15.2 Semantic classification

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
export_changed
abi_changed
```

Examples:

Same symbol hash, different preferred name:

```text
symbol_renamed
build impact: metadata_only
```

Same symbol hash, different definition hash, same signature hash:

```text
implementation_changed
build impact: recompile changed function object, relink affected outputs
```

Same symbol hash, different signature hash:

```text
interface_changed
build impact: recompile changed function and dependents, relink affected outputs
```

### 15.3 Human-readable diff projection

Example rename output:

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
  native object artifact keys

Incremental build impact:
  metadata_only
  regenerate source projections
  no object recompilation
  no relink unless export map changed
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

Incremental build impact:
  recompile object for main.total
  relink outputs that include main.total
  callers do not require recompilation because dependency interface hashes are unchanged
```

## 16. Reference evaluator and compiler backend

### 16.1 Reference evaluator

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

### 16.2 C projection

The C projection emits readable C source from a `ProgramRoot`.

Acceptable properties:

- Uses preferred display names for readability.
- Does not define native object identity.
- Does not consult the explicit public ABI export map.
- Renders deterministic declarations and definitions.
- Is allocation-free for the v0 language subset.
- Can be compiled by an external C compiler in tests.
- Can be scanned for forbidden runtime calls.

It is acceptable for a rename to change C projection function names, because the C projection is human-facing text.

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

### 16.3 Native backend

The native backend consumes lowered IR and emits object artifacts.

Required v0 native backend behavior:

```text
input:  one FunctionDef or deterministic codegen unit
output: relocatable object artifact bytes + metadata
calls:  references stable internal ABI symbols derived from symbol_hash
cache:  keyed by implementation hash, dependency interface hashes, target, ABI, and codegen options
```

The backend must not use display names for internal native symbol identity.

For v0, a native backend may support only:

```text
i64 parameters and returns
bool values represented as target integer values
unit returns
arithmetic and comparison ops
unary integer negation and boolean not
let bindings lowered to shared values
conditionals
static direct calls
one target triple
```

## 17. Incremental compilation and caching

v0 should include the cache model.

### 17.1 Cache key

A cache key must include all inputs that affect an artifact.

Example typed key payload:

```json
{
  "schema": "codedb/cache-key/v1",
  "artifact_kind": "object_file",
  "input_hash": "sha256:function-def-or-lowered-ir",
  "backend": "native-elf-v0",
  "target": "x86_64-unknown-linux-gnu",
  "compiler_version": "codedb-0.1.0",
  "pipeline_version": "pipeline:v0",
  "runtime_version": "runtime:none",
  "abi": "codedb-v0-internal",
  "codegen_options": {
    "opt_level": "none",
    "relocation_model": "static-or-pic"
  },
  "dependency_interface_hashes": [
    "sha256:callee-interface"
  ]
}
```

The cache key is:

```text
cache_key = sha256("codedb/cache/v1\0" || canonical_key_payload)
```

For no-runtime output, `runtime_version` should be:

```text
runtime:none
```

### 17.2 Required cached artifacts

Required:

```text
rendered_source
parsed_expression
typed_expression
function_dependency_set
interface_hash
implementation_hash
c_projection
lowered_ir
object_file
link_plan
```

Optional:

```text
llvm_ir
cranelift_ir
assembly
executable
source_map
debug_info
```

### 17.3 Invalidation

If only display metadata changes:

```text
regenerate projections
native object artifacts remain valid
link artifacts remain valid unless export map changed
```

If an implementation hash changes but interface hash does not:

```text
recompute the changed function lowered IR if needed
recompile the changed function object artifact
relink final binaries that include that object
callers do not need recompilation
```

If an interface hash changes:

```text
recompile the changed function
recompute direct dependents
then recursively recompute affected dependents
relink final binaries
```

If target/codegen options change:

```text
object cache miss for that target/options set
link cache miss for that target/options set
semantic objects remain unchanged
```

## 18. Build planning

A build plan is a computed description of required artifact work between roots or for a target output.

Build impact categories:

```text
metadata_only
projection_only
relink_only
recompile_symbols
recompile_dependents
full_rebuild
```

Inputs:

```text
old_root_hash, optional
new_root_hash
target triple
entry symbol
requested output kind
cache state
```

Output:

```json
{
  "impact": "recompile_symbols",
  "projection_artifacts": ["canonical_source", "c_projection"],
  "recompile": ["sha256:symbol-total"],
  "relink": true,
  "unchanged_function_defs": ["sha256:function-def-tax"],
  "reason": "implementation_hash_changed"
}
```

Rules:

```text
Names affect projections.
Internal ABI symbols derive from symbol identity.
Callee body changes affect callee object and final link products.
Callee interface changes affect callers.
Export-map changes affect link products.
Target/codegen changes affect object and link products for that target only.
```

## 19. Projections

A projection is a readable rendering of a root.

v0 needs:

```text
canonical_source
c_projection
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
Never treat projection text as the source of truth.
```

Post-v0 projections:

```text
one_file_per_function
public_api_only
dependency_order
callers_of_symbol
tests_only
literate_markdown
lowered_ir
assembly
```

## 20. Import model

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

Importing text must not give text source higher authority than the object DAG.

## 21. Verification

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
cache keys match structured artifact metadata where checkable
object artifact bytes match artifact hashes
link plans reference existing object artifacts
internal ABI names are stable and derived from symbol identity
no no-runtime projection/backend artifact contains forbidden runtime calls, where checkable
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
bad_artifact_bytes
bad_link_plan
bad_abi_symbol
forbidden_runtime_dependency
```

## 22. Rebuild from scratch

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

Cached artifacts are not source truth. They may be exported for convenience, but a correct system must be able to regenerate them from roots, migrations, target options, and compiler version.

If migrations are exported as newline-delimited JSON, the system can rebuild without copying the original SQLite file.

## 23. Branches and merge

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
build impact recomputation
```

## 24. Agent write API

Agents should primarily modify the program through structural operations.

Required v0 operation API:

```text
create_function(module, name, params_with_types, return_type, body_ast)
rename_symbol(symbol_or_name, new_name)
replace_function_body(symbol_or_name, body_ast)
change_function_signature(symbol_or_name, new_signature, callsite_updates)
delete_symbol(symbol_or_name)
create_alias(symbol_or_name, alias)
remove_alias(symbol_or_name, alias)
set_export(symbol_or_name, exported_name)
remove_export(symbol_or_name, exported_name)
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
build_impact_summary
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
    "build_impact": {
      "kind": "metadata_only",
      "regenerate": ["canonical_source", "c_projection"],
      "recompile": [],
      "relink": false
    }
  }
}
```

## 25. First demo program

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
rename tax -> vat does not change internal native ABI symbol
rename tax -> vat does not invalidate native object artifacts
export renders vat in total body
emit-c produces allocation-free C projection
body replacement invalidates only the changed function object and affected links
signature change invalidates affected dependents
replay produces identical root hash
verify passes
```

## 26. Suggested implementation milestones

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

### Milestone 5 — Projection Boundary

Deliver:

```text
canonical source export
C projection emit command
artifact kind names that distinguish projection from compiler backend
forbidden runtime call scan for C projection
```

Acceptance:

```text
emit-c produces deterministic C projection
emitted C contains no malloc/free/printf/thread calls
rename may change C projection names
rename does not change semantic symbol identity
```

### Milestone 6 — Build Planner and Cache Keys

Deliver:

```text
interface hash
implementation hash
structured cache key payload
build impact planner
dependency interface hashing
cache lookup helpers
```

Acceptance:

```text
rename is metadata_only
body change recompiles one symbol object
signature change identifies transitive dependents
cache key includes target and ABI options
```

### Milestone 7 — Lowered IR

Deliver:

```text
lowered function IR
lowering from typed expression DAG
lowered_ir cache artifacts
emit-ir command
IR verification
```

Acceptance:

```text
renames do not change lowered IR
body changes change lowered IR only for affected functions
calls use symbol hashes or stable ABI symbols, not display names
```

### Milestone 8 — Native Object Artifacts

Deliver:

```text
backend trait
object_file artifact metadata
object bytes in compile_cache.artifact_bytes
stable internal ABI names
one-function object emission for v0
```

Acceptance:

```text
unchanged functions reuse object artifacts
body-only change recompiles changed object only
rename does not recompile objects
object artifact hash matches bytes
```

### Milestone 9 — Link Plans and Executables

Deliver:

```text
LinkPlan artifact
link-native command
build command
explicit export map handling
relink planning
```

Acceptance:

```text
link plan is deterministic
relink reuses unchanged objects
export-map changes do not require recompiling function bodies
built executable returns the same value as reference evaluator for demo programs
```

### Milestone 10 — Replay and Verify

Deliver:

```text
replay from genesis
verify indexes
verify dependencies
verify types
verify history
verify cache metadata
verify object artifacts
verify link plans
```

Acceptance:

```text
replay produces the same root hash
verify passes on clean DB
verify fails on intentionally corrupted DB
```

## 27. Post-v0 roadmap

After the v0 demo and native artifact path work, likely next steps are:

```text
richer expression language
structural agent API over HTTP or MCP
local variable identity
records and enums
ownership / borrowing / region model
explicit allocation model
branch merge
migration squashing
semantic patch language
multi-target object emission
LLVM backend adapter
Cranelift backend adapter
MLIR backend adapter
Wasm backend
visual graph explorer
source maps and debug info
incremental test selection
```

## 28. Design principles

1. Files are projections.
2. C output is a projection/debug artifact, not the primary compiler backend.
3. Source truth is the object DAG.
4. The current state is identified by `root_hash`.
5. The explanation of the current state is `migration_history`.
6. Names are metadata.
7. Global references use symbol hashes, not names.
8. Internal native ABI symbols derive from stable symbol identity.
9. Symbol hashes must be stable across renames, body changes, and signature changes.
10. Function signatures are typed, content-addressed interface objects.
11. Semantic changes are migrations, not line edits.
12. Diffs should preserve intent when possible.
13. Equal hashes prove equal subgraphs.
14. The target language core is statically typed.
15. Generated programs do not require a managed runtime.
16. Compilation goes from typed/lowered semantic objects to object artifacts.
17. Linking is explicit and cached through link plans.
18. Rebuild must be deterministic.
19. Caches are disposable.
20. Verification is mandatory.

## 29. Open questions

These do not block v0, but should be revisited:

1. Should v1 use BLAKE3 instead of SHA-256?
2. Should migration logs be stored inside SQLite only, or also exported as NDJSON?
3. Should symbol birth seeds be random, agent-provided, or deterministically derived from the creating migration?
   Resolved (v3): deterministically derived from the creating migration and an
   in-migration ordinal — name-independent and fixed at birth, so identities and
   root hashes reproduce on rebuild. See [SPEC_V3.md](SPEC_V3.md) §10 and §11.
4. Should root metadata changes count as semantic root changes, or should there be separate semantic and presentation roots?
5. Should function calls bind to symbol hashes, exact definition hashes, or both at different compiler stages?
6. How should branch merge conflicts be represented?
7. Should import from text be allowed to synthesize migrations against a non-empty root?
8. How should comments/docstrings be modeled: metadata, semantic documentation objects, or projection-only text?
9. Should the first native backend use a direct ELF writer, Cranelift, LLVM, or another object-emission strategy?
10. Should the object store eventually support garbage collection of unreachable objects?
11. What ownership/borrowing/allocation model should exist post-v0?
12. Should integer overflow be defined, trapped, wrapping, or statically prevented?
13. Should bounds checks be required, optional, or statically proven for future arrays/slices?
14. Should public ABI export names be root metadata, module metadata, or separate export objects?
15. Should link plans be semantic objects or disposable cache artifacts?
16. How much debug info should be generated from semantic objects and projection metadata?
