# PLAN_V1.md — CodeDB Semantic Workspace Roadmap

Status: Draft 1.0  
Scope: implementation roadmap for the v1 semantic workspace track

## Direction

The v0 CodeDB proof of concept demonstrates that a program can be stored as immutable, content-addressed semantic objects with migration history, projections, lowered IR, native object artifacts, link plans, replay, and verification.

The v1 implementation goal is to turn that proof into a usable semantic workspace for agents and humans.

The target workflow is:

```text
inspect snapshot
  -> propose semantic operation or patch
  -> apply against expected root
  -> receive semantic summary + build impact
  -> run impacted tests
  -> trace/debug behavior
  -> build artifacts
  -> verify workspace
```

Projection files remain useful, but the primary editing surface is the semantic workspace API.

## Current implementation baseline

V1 builds on the existing implementation:

- Rust CLI and library crate.
- SQLite-backed immutable object store.
- Canonical JSON hashing.
- Program roots, symbol births, function signatures, function definitions, expression DAG objects, branches, migrations, histories, materialized indexes, and cache rows.
- Typed expression language with `i64`, `bool`, `unit`, calls, binary operations, unary operations, `let`, local references, and conditionals.
- Reference evaluator.
- Deterministic source projection and C projection.
- Lowered IR.
- Native object backends for Linux x86-64 ELF relocatable objects and Apple Silicon arm64 Mach-O relocatable objects.
- Deterministic link plans.
- Host-native executable build path where supported.
- Structural `codedb/apply/v1` JSON operations.
- JSON inspection modes for important CLI commands.
- History export/import.
- Replay and verification.
- Smoke tests for docs and example flows.

The v1 plan should not destabilize that baseline. Each phase should preserve the v0 demos and tests.

## Phase 1 — Version Boundary and Documentation Split

Goal: freeze the v0 proof-of-concept story and establish v1 as the semantic workspace design track.

Deliverables:

```text
create docs/SPEC_V1.md
create docs/PLAN_V1.md
update docs/SPEC.md header to clarify v0 scope
update docs/PLAN.md header to clarify v0 implementation-plan/status scope
update README documentation map
```

Files likely touched:

```text
docs/SPEC.md
docs/PLAN.md
docs/SPEC_V1.md
docs/PLAN_V1.md
README.md
```

Acceptance checks:

```text
README points to both v0 and v1 docs
v0 docs still describe the current implemented proof
v1 docs describe the semantic workspace track
no command behavior changes
cargo test still passes
```

## Phase 2 — Agent Workspace API

Goal: expose CodeDB as a long-lived semantic workspace that agents can inspect and mutate without relying on projection files as source truth.

Initial command:

```bash
codedb serve <db> --addr 127.0.0.1:8787
```

The first transport can be JSON-RPC over HTTP. Keep request and response structs transport-neutral so the same API can later be used by an embedded library, local socket, or MCP adapter.

Deliverables:

```text
workspace server command
transport-neutral request/response structs
stable response envelope
structured diagnostics
snapshot API
branch API
symbol inspection API
root diff API
structural apply API using existing codedb/apply/v1 operations
build-plan API
verify API
integration tests that drive the server
```

Required methods:

```text
workspace.current
workspace.branches
workspace.branch.create
workspace.branch.fast_forward
workspace.branch.delete

symbols.list
symbols.show
symbols.callers
symbols.resolve

roots.diff
roots.export_projection

ops.apply
ops.preview

build.plan

history.list

verify.run
```

Files likely touched:

```text
src/main.rs
src/lib.rs
src/api.rs
new src/workspace.rs
new src/server.rs
Cargo.toml
tests/workspace_api.rs
```

Possible dependencies:

```text
axum or tiny-http for HTTP
serde for request/response structs
uuid or deterministic request IDs if needed
```

Acceptance checks:

```text
server starts against an existing database
client can read current branch root/history
client can list and show symbols
client can apply a rename with expected_root
client receives old/new roots and history hashes
client receives structured build impact
client can run verify through the API
existing CLI commands continue to work
API tests do not require network access beyond localhost
```

## Phase 3 — Root-Bound Transactions and Conflict Responses

Goal: make concurrent agent writes safe by making expected-root behavior mandatory in the workspace API and precise in diagnostics.

Deliverables:

```text
workspace transaction type
expected_root requirement for API writes
batch-level atomicity for ops.apply
structured conflict kinds
current-root information on stale writes
idempotency token support for retried client requests
transaction tests with simulated concurrent agents
```

Conflict kinds:

```text
stale_root
name_conflict
signature_conflict
dependency_conflict
export_conflict
delete_conflict
type_error
invalid_operation
```

Files likely touched:

```text
src/api.rs
src/workspace.rs
src/migrations.rs
src/store.rs
tests/workspace_concurrency.rs
```

Acceptance checks:

```text
two agents can inspect the same root
first write against main succeeds
second write against the stale root fails with stale_root
stale_root response includes expected_root and actual_root
failed stale write does not create objects that become reachable from the branch
failed stale write does not update migrations/history/branch pointer
batch conflict rolls back the full batch
CLI --expect-root behavior remains compatible
```

## Phase 4 — Branch Workspace Operations

Goal: let agents work concurrently on cheap semantic branches before full merge exists.

Deliverables:

```text
branch create from branch or root
branch list with root/history pointers
branch delete
branch fast-forward
branch compare
branch API methods and CLI commands if useful
```

Initial commands:

```bash
codedb branch create <db> <name> --from main
codedb branch create <db> <name> --from-root <root>
codedb branch list <db> --json
codedb branch compare <db> main agent/foo --json
codedb branch fast-forward <db> main agent/foo --expect-root <old-main-root>
codedb branch delete <db> agent/foo
```

Files likely touched:

```text
src/main.rs
src/store.rs
src/api.rs
src/workspace.rs
tests/branches.rs
```

Acceptance checks:

```text
branch create copies root/history pointer only
mutating one branch does not move another branch
fast-forward succeeds only when target root matches expected root
fast-forward fails cleanly if target branch moved
branch deletion does not delete semantic objects
verify validates branch pointers after operations
```

## Phase 5 — Concurrent Artifact Jobs

Goal: make build execution parallel and safe by coordinating workers through content-addressed artifact jobs.

Deliverables:

```text
artifact_jobs table
job claim helper
job completion helper
job failure helper
build-plan to job graph conversion
parallel object compilation executor
cache-key duplicate protection
artifact status API
```

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

Job statuses:

```text
queued
running
succeeded
failed
abandoned
```

Files likely touched:

```text
schema.sql
src/artifact.rs
src/build_plan.rs
src/store.rs
src/link.rs
src/backend/native.rs
new src/jobs.rs
tests/artifact_jobs.rs
```

Acceptance checks:

```text
two workers claiming the same cache_key result in one owner
successful job writes exactly one committed artifact cache entry
duplicate object compilation cannot corrupt cache rows
failed jobs record structured error_json
subsequent builds can retry failed or abandoned jobs
parallel build of shop demo produces same link plan as serial path
rename-only change does not enqueue object compilation jobs
body-only change enqueues changed function object and downstream link work
```

## Phase 6 — Semantic Trace

Goal: make execution inspectable through deterministic semantic trace records before building an interactive debugger.

Initial command:

```bash
codedb trace <db> <entry-name> [args...] --json
```

Deliverables:

```text
trace event structs
instrumented reference evaluator
stable JSON trace schema
function enter/exit events
expression evaluation events
call events
branch/if decision events
value events
error/trap events
trace command
trace API method
trace tests for deterministic output
```

Trace location:

```text
root_hash
symbol_hash
function_def_hash
expr_hash
evaluator frame index
```

Files likely touched:

```text
src/expr.rs
src/lib.rs
src/api.rs
new src/trace.rs
tests/trace.rs
```

Acceptance checks:

```text
trace of examples/shop.cdb main returns 120
trace is deterministic across repeated runs on same root
trace events contain symbol_hash and expr_hash where applicable
rename changes display names but preserves semantic trace targets
body replacement changes trace at changed expression/function
trace failures include structured diagnostics
```

## Phase 7 — Interactive Semantic Debugger

Goal: allow humans and agents to step through CodeDB programs at the semantic DAG level.

Initial command:

```bash
codedb debug <db> <entry-name> [args...]
```

Deliverables:

```text
debug session state
breakpoints by symbol_hash
breakpoints by expr_hash
step
next
continue
backtrace
print params
print locals
show current expression
show current function
REPL-style CLI debugger
programmatic debugger API
```

Files likely touched:

```text
src/trace.rs
new src/debugger.rs
src/main.rs
src/api.rs
tests/debugger.rs
```

Acceptance checks:

```text
can step through shop main -> total -> tax
symbol breakpoint survives rename
expr breakpoint triggers on matching expression hash
removed expr breakpoint reports obsolete after body replacement
backtrace contains semantic frames
print params shows typed values
interactive command parser has non-interactive tests
```

## Phase 8 — Projection Source Maps

Goal: let source-like projections map back to semantic objects without making projection text authoritative.

Deliverables:

```text
source-map artifact schema
span builder for canonical source projection
optional source map output for export
source map API method
source map verification
```

Initial command shape:

```bash
codedb export <db> --branch main --out projection.cdb --source-map projection.cdb.map.json
```

Files likely touched:

```text
src/backend_c.rs or projection module
src/lib.rs
src/main.rs
src/artifact.rs
src/verify.rs
tests/source_maps.rs
```

Acceptance checks:

```text
export can emit projection and source map together
source map records root_hash and projection kind
function names map to symbol_hashes
expression spans map to expr_hashes
rename changes spans/display text but not symbol identity
verify catches source map entries whose root or expr_hash is impossible
```

## Phase 9 — Native Debug Metadata Scaffold

Goal: prepare native artifacts for debugger integration by mapping lowered operations to object-code ranges.

This phase does not require full DWARF.

Deliverables:

```text
lowered op IDs stable within a lowered IR artifact
mapping from expr_hash to lowered op IDs
mapping from lowered op IDs to text offsets/ranges where available
native object metadata extension for debug ranges
link-plan propagation of object debug metadata
```

Files likely touched:

```text
src/lowering.rs
src/backend/native.rs
src/link.rs
src/artifact.rs
src/verify.rs
tests/native_debug_metadata.rs
```

Acceptance checks:

```text
lowered IR contains op IDs or stable value IDs usable by debug metadata
object metadata includes text offset ranges for emitted operations where supported
metadata maps native ranges back to symbol_hash and expr_hash
rename does not change debug mappings except projection source maps
verify catches malformed debug metadata
```

## Phase 10 — Semantic Test Objects

Goal: store tests as semantic objects and run them through the evaluator and, where supported, native builds.

Deliverables:

```text
TestCase object payload
root test registry metadata
create-test operation
delete-test operation
list tests command/API
run tests command/API
reference evaluator test runner
native agreement mode where host target is linkable
JSON test results
```

Initial commands:

```bash
codedb test <db>
codedb test <db> --json
codedb create-test <db> main_returns_120 --entry main --expect-i64 120
```

Files likely touched:

```text
src/model.rs
src/migrations.rs
src/api.rs
src/lib.rs
src/main.rs
new src/tests.rs
tests/semantic_tests.rs
```

Acceptance checks:

```text
can create a semantic test for shop main == 120
test survives export/import history
test runner reports pass/fail as structured JSON
type-invalid test arguments are rejected
native agreement tests are skipped cleanly when host linker/target is unavailable
verify validates test objects and test registry references
```

## Phase 11 — Incremental Test Impact

Goal: run only tests affected by a semantic change.

Deliverables:

```text
test dependency collection
root-to-root changed symbol classification
reachable dependency analysis from test entry symbols
test-impact command/API
impact reasons
projection/export test category support
```

Initial command:

```bash
codedb test-impact <db> <old-root> <new-root> --json
```

Files likely touched:

```text
src/diff.rs
src/build_plan.rs
src/tests.rs
src/store.rs
src/main.rs
tests/test_impact.rs
```

Acceptance checks:

```text
body change in tax selects tests reaching tax
body change in unrelated symbol does not select shop main test
rename-only change does not select behavior-only tests
signature change selects tests reaching changed function and dependents
impact output explains selected/skipped reason per test
```

## Phase 12 — Semantic Patch Preview

Goal: add a higher-level patch language that matches semantic structure and previews structural operations before applying them.

Deliverables:

```text
codedb/semantic-patch/v1 schema
patch parser
matcher over symbols, definitions, expressions, types, calls, literals, and exports
preview result schema
planned operation generation
conflict reporting
typecheck preview
build impact preview
```

Initial command:

```bash
codedb patch preview <db> --json patch.json
```

Files likely touched:

```text
new src/patch.rs
src/expr.rs
src/model.rs
src/migrations.rs
src/api.rs
src/main.rs
tests/patch_preview.rs
```

Acceptance checks:

```text
preview can find literal 20 inside tax body
preview can find call targets to tax
preview returns matched expr_hashes and symbol_hashes
preview returns planned structural operations
preview does not update branch pointers or write migrations
preview reports type errors before apply
```

## Phase 13 — Semantic Patch Apply

Goal: apply semantic patches as ordinary structural migrations with provenance and build impact.

Deliverables:

```text
patch apply command/API
patch-to-operations lowering
expected_root enforcement
atomic patch application
patch provenance in migration agent metadata
patch result schema
```

Initial command:

```bash
codedb patch apply <db> --json patch.json
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

Files likely touched:

```text
src/patch.rs
src/migrations.rs
src/api.rs
src/main.rs
tests/patch_apply.rs
```

Acceptance checks:

```text
replace_literal patch can change tax rate from 20 to 18
result is represented as structural migration history
patch apply returns semantic summary and build impact
patch apply is retry-safe with expected_root
failed patch leaves branch unchanged
trace diff after patch explains changed behavior
```

## Phase 14 — Provenance and Semantic Blame

Goal: answer why semantic objects exist and which migrations last affected them.

Deliverables:

```text
blame-symbol command/API
blame-expr command/API
migration lookup by symbol/expression involvement
birth migration lookup
last body/signature/name/export change lookup
agent metadata reporting
```

Initial commands:

```bash
codedb blame-symbol <db> <symbol-or-name> --json
codedb blame-expr <db> <expr-hash> --json
```

Files likely touched:

```text
src/migrations.rs
src/store.rs
new src/provenance.rs
src/main.rs
src/api.rs
tests/provenance.rs
```

Acceptance checks:

```text
blame-symbol tax reports create_function migration
blame-symbol after rename reports rename migration separately from birth
body replacement updates last body-change provenance
blame-expr reports migration that introduced reachable expression hash
history import preserves provenance answers
```

## Phase 15 — Semantic Bisect and Why

Goal: find and explain behavior changes across migration history.

Deliverables:

```text
history predicate runner
eval-output predicate
semantic-test predicate
binary search over replayable migration history
why command comparing traces/diffs between roots
structured explanation output
```

Initial commands:

```bash
codedb bisect-history <db> main --expect-output 120 --json
codedb why <db> main --from <old-root> --to <new-root> --json
```

Files likely touched:

```text
src/provenance.rs
src/trace.rs
src/diff.rs
src/migrations.rs
src/main.rs
src/api.rs
tests/bisect.rs
```

Acceptance checks:

```text
bisect finds first migration changing shop main from 120 to 118
why reports changed function, changed expression/literal, and migration hash
bisect works after history export/import
predicate failures are structured diagnostics
```

## Phase 16 — Conservative Semantic Merge

Goal: merge simple non-conflicting branch histories using semantic diffs and migration replay, not text files.

Deliverables:

```text
common ancestor detection
semantic changed-symbol set calculation
merge preview command/API
fast conservative merge for disjoint symbol changes
rename + body-change merge case
structured conflict output
merge commit/migration representation decision
```

Initial commands:

```bash
codedb merge preview <db> main agent/foo --json
codedb merge apply <db> main agent/foo --expect-root <main-root> --json
```

Files likely touched:

```text
src/diff.rs
src/migrations.rs
src/store.rs
new src/merge.rs
src/main.rs
src/api.rs
tests/merge.rs
```

Acceptance checks:

```text
branches changing disjoint symbols merge automatically
rename tax on one branch and body-change tax on another branch can merge
same-name rename conflict is reported semantically
same export name for different symbols conflicts
same function body changed on both branches conflicts conservatively
merge result verifies
history export/import preserves merge result or chosen merge representation
```

## Phase 17 — Module Boundaries

Goal: start scaling beyond a single implicit module while keeping symbol identity stable.

Deliverables:

```text
module metadata object or root metadata extension
move-symbol operation
module list/show commands
module-aware projection ordering
module-aware name resolution
module-aware API responses
```

Initial commands:

```bash
codedb module list <db> --json
codedb module move-symbol <db> <symbol-or-name> <module> --expect-root <root> --json
```

Files likely touched:

```text
src/model.rs
src/migrations.rs
src/expr.rs
src/lib.rs
src/main.rs
src/api.rs
tests/modules.rs
```

Acceptance checks:

```text
moving a symbol between modules does not change symbol_hash
module move updates projection metadata
module move does not invalidate native object artifacts
name conflicts are scoped by module
call resolution remains symbol-hash based after import/export
```

## Phase 18 — Records and Enums

Goal: add practical data modeling while still avoiding implicit heap/runtime requirements.

Deliverables:

```text
record type objects
enum type objects
record literal expression
field access expression
enum variant construction
pattern match or case expression
type checker support
evaluator support
projection support
lowering support where feasible
native backend support or explicit unsupported diagnostics
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/model.rs
src/backend_c.rs
src/lowering.rs
src/backend/native.rs
src/migrations.rs
tests/records_enums.rs
```

Acceptance checks:

```text
record values type-check and evaluate
enum values type-check and evaluate
projection round-trips supported syntax
unsupported native lowering fails with clear diagnostics until implemented
no feature requires implicit heap allocation
verify validates type objects and expression objects
```

## Phase 19 — Effect System Scaffold

Goal: make effects explicit in function signatures before adding serious I/O, FFI, allocation, or target-language concurrency.

Deliverables:

```text
effect enum/type representation
function signature effects field
parser/projection syntax for effects
apply JSON schema support
show/list JSON includes effects
type checker verifies effect declarations where possible
build/test APIs expose effect metadata
```

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

Files likely touched:

```text
src/types.rs
src/expr.rs
src/model.rs
src/migrations.rs
src/lib.rs
src/main.rs
docs/SPEC_V1.md
tests/effects.rs
```

Acceptance checks:

```text
pure functions remain default for existing examples
signature hash changes when effect set changes
effect-only signature change has correct build impact
list/show expose effects
agents can query impure functions through API
```

## Phase 20 — FFI Boundary

Goal: model external symbols explicitly and route them through type checking, effects, and link plans.

Deliverables:

```text
ExternalFunction object
extern declaration syntax or structural apply operation
ABI tag support
link_name and library metadata
external call type checking
external effect checking
link plan external symbol/library entries
native backend call relocation support for externs
```

Initial example:

```text
extern fn puts(ptr: ptr_i8) -> i32 abi[c] effects[io, ffi] link_name "puts"
```

Files likely touched:

```text
src/model.rs
src/types.rs
src/expr.rs
src/lowering.rs
src/backend/native.rs
src/link.rs
src/migrations.rs
src/main.rs
tests/ffi.rs
```

Acceptance checks:

```text
extern function appears in list/show JSON
calls to externs type-check
extern effects propagate into caller effect validation
link plan includes external symbols
missing external link metadata is a verify failure
pure functions cannot silently call io/ffi functions without effect change
```

## Phase 21 — Bundle Export and Import

Goal: make semantic package distribution possible without treating source files as the canonical package format.

Deliverables:

```text
bundle format
root object closure export
migration slice export
optional artifact cache bundle
bundle manifest
bundle import validation
package identity by root/history/API hash
```

Initial commands:

```bash
codedb bundle export <db> --root <root> --out package.codedb.bundle
codedb bundle import <db> package.codedb.bundle
```

Files likely touched:

```text
src/store.rs
src/migrations.rs
new src/bundle.rs
src/main.rs
src/api.rs
tests/bundle.rs
```

Acceptance checks:

```text
bundle import reconstructs object closure
imported bundle verifies
tampered bundle fails validation
optional artifact cache can be ignored and regenerated
bundle does not require projection source files
```

## Phase 22 — Visual Graph Explorer

Goal: provide a compelling human view of roots, symbols, expression DAGs, dependencies, migrations, traces, tests, and artifacts.

Deliverables:

```text
read-only graph API endpoints
simple web UI or static viewer
root graph view
symbol detail view
expression DAG view
dependency graph view
migration timeline
artifact/cache view
trace view
test impact view
```

Files likely touched:

```text
src/api.rs
src/server.rs
new web/ or viewer/
tests/graph_api.rs
```

Acceptance checks:

```text
viewer can inspect shop demo without mutating DB
symbol detail shows names, signature, body, callers, ABI symbol, object artifacts
migration timeline links to semantic diffs
trace view links events to expression hashes
viewer uses same API as agents
```

## Cross-cutting requirements

Every v1 phase should preserve these invariants:

```text
semantic object hashes are deterministic
projection text is not source truth
branch updates are transactional
failed operations do not move branch pointers
structural operations are replayable
artifact cache entries are disposable
verify distinguishes corruption classes
existing README demo remains runnable
cargo test passes
```

## Suggested milestone grouping

The phases above are detailed. For implementation planning, group them into four milestones.

### Milestone A — Workspace API and Safe Concurrency

Includes:

```text
Phase 1
Phase 2
Phase 3
Phase 4
Phase 5
```

Outcome:

```text
multiple agents can inspect and mutate branches safely, and builds can execute through coordinated artifact jobs
```

### Milestone B — Debug, Trace, and Tests

Includes:

```text
Phase 6
Phase 7
Phase 8
Phase 9
Phase 10
Phase 11
```

Outcome:

```text
program behavior is inspectable, debuggable, and testable through semantic identities
```

### Milestone C — Patches, Provenance, and Merge

Includes:

```text
Phase 12
Phase 13
Phase 14
Phase 15
Phase 16
```

Outcome:

```text
agents can make higher-level semantic edits, explain changes, bisect behavior, and merge simple branches without text diffs
```

### Milestone D — Scale the Language and Repository Model

Includes:

```text
Phase 17
Phase 18
Phase 19
Phase 20
Phase 21
Phase 22
```

Outcome:

```text
CodeDB starts growing from a proof/demo language into a richer semantic repository system
```

## Recommended immediate next PRs

### PR 1 — Add v1 docs and version boundary

Files:

```text
docs/SPEC_V1.md
docs/PLAN_V1.md
README.md
docs/SPEC.md
docs/PLAN.md
```

Acceptance:

```text
v0/v1 distinction is clear
README links to v1 docs
no code behavior changes
```

### PR 2 — Workspace API types without server transport

Files:

```text
src/api.rs
new src/workspace.rs
tests/workspace_types.rs
```

Acceptance:

```text
request/response structs serialize deterministically
envelope and error schemas are stable
ops.apply API can call existing apply implementation directly in-process
```

### PR 3 — Minimal local server

Files:

```text
Cargo.toml
src/main.rs
src/server.rs
src/workspace.rs
tests/workspace_api.rs
```

Acceptance:

```text
codedb serve starts
workspace.current works
symbols.list works
symbols.show works
ops.apply rename works with expected_root
verify.run works
```

### PR 4 — Concurrent write tests

Files:

```text
src/workspace.rs
src/migrations.rs
tests/workspace_concurrency.rs
```

Acceptance:

```text
simulated stale write fails with stale_root
atomic batch rollback is proven
branch pointer update is serialized
```

## V1 definition of done

V1 should be considered complete when this end-to-end flow works:

```text
start workspace server
create branch agent/foo from main
inspect symbols through API
apply semantic patch through API against expected root
receive semantic summary and build impact
run semantic trace
run impacted tests
build with parallel artifact jobs
verify workspace
fast-forward main if still at expected root
bisect history to explain a behavior change
merge a simple non-conflicting branch
export projection and source map for review
```

V1 does not need to be a production language. It needs to prove that semantic code workspaces are practical for agents.
