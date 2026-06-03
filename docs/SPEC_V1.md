# SPEC_V1.md — CodeDB Semantic Workspace

Status: Draft 1.0  
Scope: v1 design track built on the v0 CodeDB proof of concept

## 1. Thesis

CodeDB v1 turns the v0 semantic object database into a concurrent, agent-oriented programming workspace.

A program is still not primarily a collection of files. A program is an immutable, content-addressed semantic DAG plus a replayable migration history. V1 adds the workspace layer that lets humans and agents inspect, edit, debug, test, build, merge, and explain that program through semantic operations.

Projection files remain human-readable views. They are useful for review, copy/paste, editor integration, and debugging context, but they are not the source of truth.

The v1 workspace identity is:

```text
repository_id + branch_name + root_hash + migration_history_head
```

Where:

- `repository_id` identifies the local or distributed CodeDB repository.
- `branch_name` identifies a mutable pointer.
- `root_hash` identifies the complete semantic program state at that pointer.
- `migration_history_head` identifies the replayable history that produced that state.

The central v1 question is:

```text
Can agents safely use CodeDB as a semantic coding workspace instead of editing files?
```

## 2. Relationship to v0

V0 proves the core model:

- immutable content-addressed objects;
- deterministic object hashing;
- SQLite-backed object storage;
- program roots;
- stable symbol identity;
- names as metadata;
- structural migrations;
- migration history;
- typed expression DAGs;
- reference evaluation;
- canonical source projection;
- C projection as a debug/projection artifact;
- lowered IR;
- native object artifacts;
- explicit link plans;
- artifact caches;
- history export/import;
- replay;
- verification.

V1 should not replace those concepts. It should make them usable as a live programming substrate.

V0 asks:

```text
Can semantic roots, migrations, projections, native artifacts, replay, and verify work?
```

V1 asks:

```text
Can multiple agents and humans use semantic roots, migrations, traces, tests, patches, branches, and artifacts as a practical workspace?
```

## 3. Design principles

1. Semantic objects remain the source of truth.
2. Projection text remains a view.
3. Agents write structural operations, not line edits, whenever possible.
4. Every write is root-aware.
5. Long-lived agent thinking must not hold workspace locks.
6. Immutable objects are freely shareable across branches, agents, and build workers.
7. Mutable branch pointers are updated transactionally.
8. Artifact production is content-addressed and safe to parallelize.
9. Debugging should start at the semantic DAG, then project to source spans and native locations.
10. Tests should be semantic objects, not only external scripts.
11. Patches should preserve intent at the symbol, type, call, and expression level.
12. Provenance should answer why a semantic object exists, not only who edited a line.
13. Merge should be conservative and semantic before it becomes clever.
14. Effects should be explicit before the target language grows concurrency or I/O.
15. Verification remains mandatory.

## 4. Workspace model

A v1 workspace is a live interface over one CodeDB repository.

```text
Workspace
  repository metadata
  branches
  current snapshots
  object store
  migration history
  artifact cache
  artifact jobs
  semantic test registry
  debug traces
  optional projection files
```

A workspace server may be a local process, embedded library, JSON-RPC server, HTTP server, or future MCP-style integration. The transport is not the core abstraction. The core abstraction is root-bound semantic operations.

### 4.1 Snapshot

A snapshot is a stable read view:

```json
{
  "schema": "codedb/snapshot/v1",
  "branch": "main",
  "root_hash": "sha256:...",
  "history_hash": "sha256:..."
}
```

Agents should inspect a snapshot, reason externally, and submit writes against that same root hash.

### 4.2 Agent session

An agent session records optional metadata for operations:

```json
{
  "schema": "codedb/agent-session/v1",
  "agent_id": "agent:example",
  "purpose": "rename tax and update callers",
  "started_at": "timestamp-outside-semantic-objects",
  "request_id": "opaque-idempotency-token"
}
```

Agent session metadata must not affect semantic object hashes. It may affect migration metadata and audit logs.

### 4.3 Transaction

A workspace transaction is an atomic batch of semantic operations against an expected root:

```json
{
  "schema": "codedb/workspace-transaction/v1",
  "branch": "main",
  "expected_root": "sha256:...",
  "operations": [
    { "kind": "rename_symbol", "name": "tax", "new_name": "vat" }
  ],
  "agent": {
    "agent_id": "agent:example"
  }
}
```

A transaction either commits completely or does not change the branch.

## 5. Agent Workspace API

V1 should expose the existing v0 capabilities through a stable machine API.

Initial command:

```bash
codedb serve <db> --addr 127.0.0.1:8787
```

The first implementation may use JSON-RPC over HTTP. The API shape should remain transport-neutral so an embedded library or MCP adapter can reuse the same request and response schemas.

### 5.1 Method families

Required method families:

```text
workspace.current
workspace.branches
workspace.branch.create
workspace.branch.fast_forward
workspace.branch.delete
workspace.branch.compare

symbols.list
symbols.show
symbols.callers
symbols.resolve

roots.diff
roots.export_projection
roots.export_source_map

ops.apply
ops.preview

build.plan
build.execute
build.artifact_status

trace.run
debug.run
trace.diff

tests.list
tests.run
tests.impact

history.list
history.export
history.import
history.bisect

verify.run
```

### 5.2 API response envelope

Every method should return a stable envelope:

```json
{
  "schema": "codedb/response/v1",
  "status": "ok",
  "result": {},
  "diagnostics": [],
  "snapshot": {
    "branch": "main",
    "root_hash": "sha256:...",
    "history_hash": "sha256:..."
  }
}
```

Errors should be structured:

```json
{
  "schema": "codedb/response/v1",
  "status": "error",
  "error": {
    "kind": "stale_root",
    "message": "branch main moved before transaction commit",
    "expected_root": "sha256:old",
    "actual_root": "sha256:new"
  },
  "diagnostics": []
}
```

### 5.3 Apply response

`ops.apply` should accept the existing `codedb/apply/v1` operation vocabulary and return a workspace-level result:

```json
{
  "schema": "codedb/apply-result/v1",
  "status": "applied",
  "branch": "main",
  "old_root_hash": "sha256:old-root",
  "new_root_hash": "sha256:new-root",
  "old_history_hash": "sha256:old-history",
  "new_history_hash": "sha256:new-history",
  "operations": [
    {
      "status": "applied",
      "operation_kind": "rename_symbol",
      "migration_hash": "sha256:migration",
      "semantic_summary": {
        "symbol": "sha256:symbol",
        "from": "main.tax",
        "to": "main.vat"
      },
      "typecheck": {
        "status": "unchanged"
      },
      "build_impact": {
        "kind": "metadata_only",
        "projection_artifacts": ["canonical_source", "c_projection"],
        "recompile": [],
        "relink": false
      }
    }
  ],
  "diagnostics": []
}
```

## 6. Concurrency model

V1 concurrency is primarily workspace concurrency, not target-language concurrency.

### 6.1 Immutable object sharing

Objects are immutable. Reads can happen concurrently without coordination beyond SQLite snapshot semantics.

### 6.2 Mutable branch pointers

Branch pointers are mutable and must be updated transactionally.

Write algorithm:

```text
read snapshot: branch -> root_hash + history_hash
agent prepares structural operations
begin transaction
reload branch pointer
if branch.root_hash != expected_root:
  rollback and return stale_root
apply operations
write new objects
write migrations
write history rows
refresh indexes
update branch pointer
commit
```

No agent should hold a write lock while reasoning outside the transaction.

### 6.3 Conflict classes

Required conflict kinds:

```text
stale_root
name_conflict
signature_conflict
dependency_conflict
export_conflict
delete_conflict
type_error
invalid_operation
cache_conflict
```

`stale_root` means the branch moved. It does not necessarily mean the semantic operation is invalid. The caller may rebase, retry against a new root, or create a branch.

### 6.4 Branch operations

V1 should support cheap branch pointers:

```text
branch create <new> --from <root-or-branch>
branch fast-forward <target> <source> --expect-root <old-target-root>
branch delete <branch>
branch compare <left> <right>
```

The first merge operation should be fast-forward only. Semantic merge comes later.

## 7. Concurrent artifact jobs

Artifact generation should be parallel and content-addressed.

A build planner produces deterministic jobs:

```json
{
  "schema": "codedb/artifact-job/v1",
  "kind": "compile_object",
  "cache_key": "sha256:...",
  "artifact_kind": "object_file",
  "input_hash": "sha256:function-def-or-lowered-ir",
  "symbol_hash": "sha256:symbol",
  "target_triple": "x86_64-unknown-linux-gnu",
  "backend": "native-elf-x86_64-v0"
}
```

Workers claim jobs by cache key. At most one committed artifact should exist for a cache key.

Suggested table:

```sql
CREATE TABLE IF NOT EXISTS artifact_jobs (
    cache_key TEXT PRIMARY KEY,
    artifact_kind TEXT NOT NULL,
    status TEXT NOT NULL,
    worker_id TEXT,
    started_at TEXT,
    finished_at TEXT,
    error_json TEXT
);
```

A worker may compile duplicate bytes speculatively, but committed cache state must remain deterministic.

Job statuses:

```text
queued
running
succeeded
failed
abandoned
```

Artifact caches remain disposable. Job rows are coordination metadata, not semantic truth.

## 8. Semantic debugger and traces

V1 should implement semantic debugging before native debugger integration.

The canonical debug location is:

```text
root_hash + symbol_hash + function_def_hash + expr_hash + evaluator_frame
```

Not:

```text
file path + line number + column
```

Line and column are projection metadata that can be mapped back to semantic objects.

### 8.1 Trace command

Initial command:

```bash
codedb trace <db> <entry-name> [args...] --json
```

Trace output:

```json
{
  "schema": "codedb/trace/v1",
  "root_hash": "sha256:...",
  "entry_symbol": "sha256:...",
  "entry_name": "main.main",
  "args": [],
  "result": { "kind": "i64", "value": "120" },
  "events": [
    {
      "event": "enter_function",
      "frame": 0,
      "symbol_hash": "sha256:...",
      "function_def_hash": "sha256:..."
    },
    {
      "event": "eval_expr",
      "frame": 0,
      "expr_hash": "sha256:...",
      "expr_kind": "call",
      "type_hash": "sha256:type-i64"
    },
    {
      "event": "return",
      "frame": 0,
      "value": { "kind": "i64", "value": "120" }
    }
  ]
}
```

### 8.2 Interactive debug command

Initial command:

```bash
codedb debug <db> <entry-name> [args...]
```

Minimum commands:

```text
step
next
continue
break symbol <name-or-hash>
break expr <expr-hash>
print params
print locals
print value <id-or-expr>
backtrace
where
show expr
show function
quit
```

### 8.3 Breakpoint identity

Semantic breakpoints should target stable semantic identities:

```text
symbol_hash
function_def_hash
expr_hash
operation kind + symbol_hash
```

A rename should not invalidate a symbol breakpoint. A body replacement may invalidate expression breakpoints attached to removed expression hashes.

### 8.4 Projection source maps

Projection commands should optionally emit source maps:

```bash
codedb export <db> --branch main --out projection.cdb --source-map projection.cdb.map.json
```

Source map shape:

```json
{
  "schema": "codedb/source-map/v1",
  "root_hash": "sha256:...",
  "projection_kind": "canonical_source",
  "projection_path": "projection.cdb",
  "spans": [
    {
      "start": { "line": 3, "column": 28 },
      "end": { "line": 3, "column": 40 },
      "symbol_hash": "sha256:...",
      "function_def_hash": "sha256:...",
      "expr_hash": "sha256:...",
      "type_hash": "sha256:..."
    }
  ]
}
```

### 8.5 Native debug metadata

Native debug support should be layered after semantic tracing.

The native mapping chain is:

```text
machine address
  -> object artifact
  -> symbol_hash
  -> lowered op id
  -> expr_hash
  -> projection source span
```

V1 does not require full DWARF in the first debugger milestone. It should first store enough object metadata to map code offsets to lowered operations and semantic expressions.

## 9. Semantic tests

Tests should be semantic objects so they can participate in dependency analysis, replay, provenance, and impact selection.

Initial test object:

```json
{
  "schema": "codedb/test-case/v1",
  "name": "shop.main_returns_120",
  "entry_symbol": "sha256:symbol-main",
  "args": [],
  "expected": { "kind": "i64", "value": "120" },
  "mode": "reference_eval"
}
```

Required commands:

```bash
codedb test <db>
codedb test <db> --json
codedb test-impact <db> <old-root> <new-root> --json
```

### 9.1 Test impact

Test impact uses the dependency graph.

Rules:

```text
if a changed symbol is reachable from a test entry symbol:
  select the test
else:
  skip the test as unaffected
```

Body-only changes should select tests that reach the changed function. Rename-only changes should not select behavior tests unless the test asserts projection output or export-map behavior.

### 9.2 Native agreement tests

Where supported, a semantic test may request native agreement:

```json
{
  "mode": "reference_and_native",
  "target_triple": "x86_64-unknown-linux-gnu"
}
```

The evaluator and native executable result must match.

## 10. Semantic patch language

The semantic patch language is the next layer above raw structural operations.

A semantic patch matches program structure and emits structural operations.

Patch input example:

```json
{
  "schema": "codedb/semantic-patch/v1",
  "branch": "main",
  "expected_root": "sha256:...",
  "match": {
    "kind": "call",
    "target_symbol": "sha256:old-callee"
  },
  "replace": {
    "kind": "call",
    "target_symbol": "sha256:new-callee",
    "args": "$same_args"
  }
}
```

Patch targets:

```text
symbol_hash
function_def_hash
function_sig_hash
expr_hash
type_hash
call target
literal value
operator kind
dependency edge
export binding
```

Initial patch operations:

```text
replace_literal
replace_call_target
rename_symbol
extract_function
inline_function
add_parameter
remove_unused_symbol
set_export
remove_export
```

Patch application must support preview:

```bash
codedb patch preview <db> --json patch.json
codedb patch apply <db> --json patch.json
```

Preview returns:

```text
matched symbols
matched expressions
planned structural operations
typecheck outcome
build impact
conflicts
```

## 11. Provenance, blame, and bisect

V1 should make semantic provenance first-class.

Questions CodeDB should answer:

```text
Who introduced this symbol?
Which migration last changed this function body?
Which migration introduced this expression hash?
Why did main change from 120 to 118?
Which agent changed this call target?
Which root first failed this test?
```

Required commands:

```bash
codedb blame-symbol <db> <symbol-or-name> --json
codedb blame-expr <db> <expr-hash> --json
codedb why <db> <entry-name> --from <old-root> --to <new-root> --json
codedb bisect-history <db> <entry-name> --expect-output <value> --json
```

### 11.1 Blame model

Semantic blame should use migrations, not source lines.

Blame output should include:

```text
symbol_hash
current root
birth migration
last signature migration
last body migration
last rename migration
last export migration
agent metadata
```

### 11.2 History bisect

History bisect replays migrations to find the first migration where a predicate changes.

Predicates:

```text
eval output equals value
test passes
test fails
symbol exists
expression exists
build impact contains symbol
```

## 12. Semantic merge

V1 merge should be conservative and semantic.

First merge algorithm:

```text
find common ancestor root
compute semantic diff ancestor -> left
compute semantic diff ancestor -> right
if changed symbol sets are disjoint:
  replay both migration sequences
else:
  return conflict with semantic explanation
```

Mergeable cases:

```text
add different symbols
rename symbol A + replace body of same symbol A
create alias + body change
set export + body change when internal ABI is stable
body changes to disjoint symbols
```

Conflict cases:

```text
rename two symbols to the same display name
set same export name for different symbols
change same function body in both branches
change signature while another branch changes call sites
force delete a symbol changed by another branch
```

Later merge versions may operate below symbol granularity using expression hashes.

## 13. Modules and package boundaries

V1 should begin separating large programs into semantic modules and packages.

Objects:

```text
ModuleDef
PackageDef
ImportBinding
VisibilityBinding
PublicApiSurface
```

Rules:

```text
module paths are metadata
symbol hashes remain identity
public API is explicit
native exports remain explicit ABI bindings
imports bind to symbols or package API hashes
```

Initial commands:

```bash
codedb module list <db>
codedb module move-symbol <db> <symbol> <module>
codedb package export <db> --root <root> --out package.codedb.bundle
codedb package import <db> package.codedb.bundle
```

## 14. Richer type and expression surface

V1 may expand the language only where it exercises the semantic model and remains compatible with runtime-free compilation.

Preferred order:

```text
records
enums
pattern matching
fixed arrays
pointers/references
explicit allocation regions
```

Avoid early:

```text
closures requiring allocation
implicit heap allocation
garbage collection
exceptions
async runtime
thread runtime
dynamic reflection
dynamic dispatch
```

## 15. Effect system

Effects should become part of function signatures before the target language gains I/O, allocation, FFI, or concurrency.

Initial effects:

```text
pure
trap
io
state
alloc
ffi
concurrent
```

Example:

```text
fn total(subtotal: i64) -> i64 effects[pure]
fn read_counter() -> i64 effects[io]
fn malloc_bytes(n: i64) -> ptr effects[alloc, ffi]
```

Effects matter for:

```text
type checking
review
agent safety
build planning
optimization
test isolation
concurrency design
FFI boundaries
```

## 16. FFI and ABI boundary

FFI should be explicit semantic data.

External function object:

```json
{
  "schema": "codedb/external-function/v1",
  "name": "puts",
  "link_name": "puts",
  "abi": "c",
  "params": ["sha256:type-ptr-i8"],
  "return": "sha256:type-i32",
  "effects": ["io", "ffi"],
  "library": "libc"
}
```

Link plans should include external symbols and libraries. Calls to external functions must pass through the type and effect system.

## 17. Visual graph explorer

A visual graph explorer is not required for correctness, but it is valuable for understanding and demos.

It should show:

```text
ProgramRoot
symbols
names
function definitions
expression DAGs
dependency graph
migration timeline
artifact cache
link plans
tests
traces
```

The graph explorer should consume the same workspace API as agents.

## 18. Verification additions

V1 verification should extend v0 verification with:

```text
workspace transaction invariants
branch pointer invariants
artifact job consistency
trace schema validation
source map span validity
semantic test object validation
semantic patch replay validation
module/package import validation
effect annotation validity
FFI link metadata validity
merge result validation
```

Verification should remain able to distinguish semantic corruption from disposable cache/job corruption.

## 19. V1 non-goals

V1 should not attempt to deliver all of the following at once:

```text
production-grade IDE
full LSP
production package registry
distributed multi-writer database
production optimizer
full DWARF emission in the first debugger milestone
advanced target-language concurrency
async runtime
garbage collector
trait/interface system
generic type system
full source round-tripping
comment preservation
```

These may become future tracks after the semantic workspace is proven.

## 20. V1 success criteria

V1 is successful when a human or agent can:

```text
open a CodeDB workspace
inspect the current root and symbols
apply root-bound structural operations through an API
receive semantic summaries and build impact
step through program execution semantically
run impacted tests after a change
build artifacts concurrently without cache corruption
trace behavior changes to migrations
bisect history for a changed result
create branches and fast-forward them safely
merge simple non-conflicting semantic changes
export projections and source maps for human review
verify the whole workspace
```

The most important success criterion is not language richness. It is whether CodeDB becomes a safer and more explainable substrate for agentic code change than editing files.
