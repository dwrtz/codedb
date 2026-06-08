# PLAN_V2.md — CodeDB Native Semantic Programming Roadmap

Status: Draft 1.0
Scope: implementation roadmap for the v2 native semantic programming track

## Direction

V2 turns the v1 semantic workspace into a native semantic compiler for useful programs.

The v2 target workflow is:

```text
model program semantically
  -> apply structural operation or patch
  -> verify types, regions, borrows, moves, effects, and layouts
  -> lower to memory-aware IR
  -> compile to native object artifacts
  -> link executable or library
  -> run native-required semantic tests
  -> trace/debug through semantic locations
  -> verify replay and artifacts
```

The central implementation rule is:

```text
No runtime/interpreter fallback for feature completion.
```

The reference evaluator may remain as an oracle. It is not the acceptance backend for v2 features.

## Current implementation baseline

V2 builds on the v1 workspace and compiler baseline:

```text
Rust CLI and library crate
SQLite-backed immutable object store
canonical JSON hashing
program roots and branches
stable symbol identity
function signatures and definitions
typed expression DAG objects
structural migrations and histories
workspace/server API surface
semantic tests and test impact direction
trace/debug/provenance direction
artifact cache and artifact jobs
lowered IR
native object backends for existing scalar/function features
link plans and host-native executable path where supported
replay, export/import, and verify
```

V2 should preserve existing demos and tests while expanding the semantic language and compiler.

## Native-done definition

A v2 feature is done only when these checks are satisfied:

```text
semantic object payloads exist
canonical hashing is deterministic
object edges are indexed
projection syntax exists
structural apply JSON exists
semantic patch support exists only for features exposed through the semantic
patch language; otherwise structural apply JSON is the mutation surface
type checking succeeds/fails deterministically
region/borrow/move/drop checking exists where applicable
effect checking exists where applicable
reference evaluator behavior exists where practical as oracle
trace/debug events use semantic identities
lowered IR represents the feature
native object backend compiles the feature
ABI/layout rules are deterministic
native-required tests pass
verify validates the feature
replay/export/import preserve the feature
```

If native-required tests cannot run because codegen is missing, the feature is not done.

## Acceptance programs

V2 should be driven by native acceptance programs under `examples/v2/`.

Required programs:

```text
line_view_refs.cdb
  references stored in records

mutable_cursor.cdb
  mutable references stored in records and state effects

invoice_static.cdb
  records, enums, fixed arrays/slices, references, loops/folds

parser_or_word_count.cdb
  byte/string slices, bounds checks, loops, later I/O

todo_cli.cdb
  capstone with args, stdout, files, strings, dynamic allocation, result handling
```

Each acceptance program should have:

```text
source projection fixture
apply JSON fixture where useful
native-required semantic tests
trace/debug fixture
verify fixture
replay/export/import fixture
```

## Phase 1 — Version Boundary and Native-Done Docs

Goal: establish v2 as the native semantic programming track and document the no-interpreter completion rule.

Deliverables:

```text
create docs/SPEC_V2.md
create docs/PLAN_V2.md
update README documentation map
add examples/v2/README.md
add native-done checklist to docs
mark v2 feature gates as native-required
```

The native-done gate is documented in [NATIVE_DONE.md](NATIVE_DONE.md). Every
future v2 feature gate is native-required: evaluator-only behavior may be used
as an oracle, but it cannot close a v2 feature gate.

Files likely touched:

```text
docs/SPEC_V2.md
docs/PLAN_V2.md
README.md
examples/v2/README.md
```

Acceptance checks:

```text
README points to v0/v1/v2 docs
v2 docs explicitly forbid interpreter fallback for feature completion
v2 acceptance programs are named
existing tests still pass
no command behavior changes required in this phase
```

## Phase 2 — Native-Required Test Harness

Goal: make it impossible to accidentally count evaluator-only behavior as v2 done.

Status: implemented. The harness supports v2 test cases with
`mode: "reference_and_native"` and `native_required: true`, reports structured
native results, and fails native-required tests when native execution is
unsupported.

Deliverables:

```text
native_required test flag
native_required failure behavior when backend unsupported
structured native test result schema
native agreement result comparison for structured values, initially scalar-compatible
feature unsupported diagnostics
CI/smoke test labels for v2 native tests
```

Initial schema direction:

```json
{
  "schema": "codedb/test-case/v2",
  "mode": "reference_and_native",
  "native_required": true
}
```

Files likely touched:

```text
src/model.rs
src/tests.rs
src/main.rs
src/api.rs
src/workspace.rs
tests/native_required.rs
```

Acceptance checks:

```text
native_required scalar test passes for existing native-supported feature
native_required test fails if native backend reports unsupported_feature
JSON result distinguishes fail, unsupported, skipped, and native_mismatch
existing non-native-required tests remain compatible
```

## Phase 3 — TypeDef, RecordDef, EnumDef, and Region Parameters

Goal: add stable named type identities and region parameters before adding memory-heavy features.

Status: implemented. The root model now has `TypeDef`, `RecordDef`, and
`EnumDef` objects, stable type/member/region identities, type-name indexes,
record/enum projection syntax, root-aware named type resolution, structural
type/member migrations, and verification for duplicate members and invalid
region references.

Deliverables:

```text
TypeDef object
RecordDef object
EnumDef object
stable field identities
stable variant identities
region parameter representation
record/enum projection syntax
root type registry
create_type / rename_type / move_type operations
add_field / rename_field / remove_field operations
add_variant / rename_variant / remove_variant operations
```

Initial projection examples:

```text
record Line {
  price_cents: i64
  qty: i64
}

record LineView<'a> {
  line: &'a Line
}

enum Discount {
  None: unit
  Percent: i64
  Fixed: Money
}
```

Files likely touched:

```text
src/model.rs
src/types.rs
src/expr.rs
src/migrations.rs
src/api.rs
src/lib.rs
src/verify.rs
src/backend_c.rs or projection module
tests/type_defs.rs
tests/type_def_migrations.rs
```

Acceptance checks:

```text
can define named records and enums
can define region-parameterized records
field and variant identities are stable across rename
root indexes include type definitions
projection round trip preserves semantic identities
verify catches duplicate fields/variants and invalid region references
existing function-only programs still work
```

## Phase 4 — Target-Independent Layout Model

Goal: compute deterministic native layouts for v2 types.

Status: implemented. V2 type layouts now compute deterministic scalar, record,
enum, reference, raw pointer, and fixed-array metadata with target-specific
size/alignment, ABI classification, copy/move/drop scaffold classification,
layout cache keys, and verification that recomputes cached layout artifacts.

The remaining SPEC §8 layout kinds — `box`, `slice`, and
`static_string_or_bytes_view` — are deferred to the phases that introduce those
types (Phases 12, 15, 17). The `contains_box` and `contains_capability_handle`
classification flags are emitted but always `false` until those types exist; no
type the current language can express sets them. The layout cache key versions
on the backend id tag (`type-layout:v2`); `LAYOUT_VERSION` carries the same tag,
guarded by `layout_cache_key_is_versioned` so a layout-format bump invalidates
cached layouts.

Deliverables:

```text
TypeLayout artifact/schema
layout computation for scalars
layout computation for records
layout computation for enums
layout computation for references
layout computation for raw pointers
layout computation for fixed arrays
layout cache key versioning
copy/move/drop classification scaffold
layout verification
```

Suggested schema:

```text
codedb/type-layout/v2
```

Files likely touched:

```text
new src/layout.rs
src/types.rs
src/model.rs
src/artifact.rs
src/backend/native.rs
src/verify.rs
tests/layout.rs
```

Acceptance checks:

```text
record field offsets are deterministic
reference fields lower to pointer-sized fields
copy/move/drop classification is deterministic
enum tag/payload layout is deterministic
layout cache key includes target_triple and layout_version
verify recomputes layouts and catches malformed layout artifacts
```

## Phase 5 — Place Model and Memory-Aware Lowered IR Scaffold

Goal: introduce the IR foundation required for references, field addresses, loads, stores, moves, and drops.

Status: implemented. Lowered IR v2 now includes semantic `Place`
representations, address-producing ops for params/locals/fields/indexes,
explicit `load`/`store`/`copy`/`move`/`drop` scaffolding, borrow debug metadata,
addressable local slots, verifier tracking for value ids versus address ids,
and scalar native backend support for the new stack-address/load/store lowering
path. Aggregate field/index address ops are inspectable and verifiable IR
scaffold; full native aggregate codegen remains in later phases.

Deliverables:

```text
Place representation
Address/value distinction in lowered IR
local stack slots for addressable values
addr_of_local
addr_of_param
addr_of_field
addr_of_index scaffold
load
store
copy
move
drop scaffold
borrow/debug metadata ops
IR verifier updates
```

Files likely touched:

```text
src/lowering.rs
src/backend/native.rs
src/backend_c.rs or projection module
src/verify.rs
src/trace.rs
src/debugger.rs
tests/lowered_memory_ir.rs
```

Acceptance checks:

```text
existing scalar functions lower through new IR without regression
record field access can be represented as place/address/load
IR verifier rejects load/store with incompatible layout/type
lowered IR inspection includes semantic expr_hash mappings
native backend can ignore borrow metadata while preserving debug maps
```

## Phase 6 — Shared References

Goal: support read-only references as semantic types and native pointers.

Status: implemented. Shared references now have projection syntax, function
region parameters, typed borrow expressions, deref/field-read lowering, record
field storage, trace support, local-borrow escape rejection, and native pointer
codegen for the `line_view_refs.cdb` acceptance program.

Deliverables:

```text
&'a T type
function region parameters
borrow_shared expression/place operation
deref read
shared reference function parameters
shared references as record fields
shared loan tracking
shared reference projection syntax
native pointer lowering
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/migrations.rs
src/lowering.rs
src/backend/native.rs
src/verify.rs
src/trace.rs
tests/shared_refs.rs
examples/v2/line_view_refs.cdb
```

Acceptance checks:

```text
LineView<'a> { line: &'a Line } compiles natively
line_total(LineView) returns correct native result
shared reference field access does not copy the full record
function cannot return reference to local
verify validates shared loan regions
trace maps dereference and field read to semantic place
```

## Phase 7 — Mutable References, Assignment, and State Effects

Goal: support exclusive mutable references and assignment through semantic places.

Status: implemented. Mutable references now have projection syntax and typed
`borrow_mut` expressions, assignment expressions returning `unit`, exclusive
loan verification, state-effect enforcement, mutable-reference record fields,
move-only layout classification, trace support, memory-aware IR lowering through
`borrow_mut`/`deref_mut`/`store`, and native pointer store codegen for the
`mutable_cursor.cdb` acceptance program.

Deliverables:

```text
&'a mut T type
borrow_mut operation
assignment expression/statement form
store through mutable reference
exclusive loan checker
state effect propagation
mutable references as record fields
move-only classification for mutable references
native store lowering
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/migrations.rs
src/lowering.rs
src/backend/native.rs
src/verify.rs
src/trace.rs
tests/mutable_refs.rs
examples/v2/mutable_cursor.cdb
```

Acceptance checks:

```text
LineEditor<'a> { line: &'a mut Line } compiles natively
mutation through editor changes native result
compiler rejects two mutable editors for the same place
compiler rejects shared read while mutable loan is live
state effect is required and reported
verify validates exclusive mutable loans
```

## Phase 8 — Copy, Move, Drop, and Loan-Carrying Records

Goal: make records with references and owned fields safe to move, copy, and drop.

Status: implemented. Copy/move/drop classification is recomputed from v2
layout metadata during semantic verification and lowered IR verification.
Shared-reference records remain Copy and duplicate their carried loans when
copied. Mutable-reference records are move-only; moving one transfers its loan
to the new owner, using the old owner is rejected as `bad_move`, and lexical
drop/end-of-scope removes the carried loan. Lowering emits explicit `copy`,
`move`, and `drop` scaffolding for whole-slot moves of named `let` bindings and
parameters.

The drop scaffold drops whole owned slots only and emits one unconditional drop
per slot. Cases that would need conditional or field-granular drop glue — an
asymmetric conditional move (moving an owned value in only one `if` branch) and
a partial move (moving a move-only value out of a record field or array
element) — are rejected fail-closed at lowering until that glue lands (Phase
15), so no latent double-drop or skipped-drop survives into native codegen. The
lowered `drop` op currently generates no machine code; real drop glue is Phase
15.

Deliverables:

```text
Copy classification
move-only classification
needs-drop classification
move expression/lowering behavior
use-after-move detection
drop insertion scaffold
loan movement for reference-containing records
loan end on drop
structured diagnostics for move/borrow errors
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/lowering.rs
src/migrations.rs
src/verify.rs
src/trace.rs
tests/move_drop.rs
tests/loan_records.rs
```

Acceptance checks:

```text
records containing shared refs are Copy when all fields are Copy
records containing mutable refs are move-only
moving a mutable cursor transfers the loan
using moved move-only value is rejected
loan ends at drop/end of scope
verify recomputes copy/move/drop classification
```

## Phase 9 — Native Records End-to-End

Goal: compile records, field access, record parameters, and record returns to native code.

Status: implemented. Record literals now lower into addressable native stack
storage, record field reads and writes use layout-driven byte/word access,
record parameters cross the internal native ABI by indirect pointer, and record
returns use hidden return slots. Lowered IR carries recomputed type layout and
ABI metadata, native backends copy aggregate byte ranges explicitly, and
native-required record tests cover small records, large records, and record
returns. Reference-carrying records are covered through native object emission
and native-required scalar entrypoints, since semantic test values do not encode
references for direct record-value comparison.

Deliverables:

```text
record literal lowering
field access lowering
field assignment lowering where mutable
record parameter ABI classification
record return ABI classification
hidden return slot support where required
record equality only if explicitly supported
record native test serialization
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/lowering.rs
src/backend/native.rs
src/link.rs
src/tests.rs
src/verify.rs
tests/native_records.rs
```

Acceptance checks:

```text
small record passed by value compiles native
large record passed indirectly where required
record returned from function compiles native
record containing reference field compiles native
native-required record tests pass
verify catches invalid field offsets and ABI metadata
```

## Phase 10 — Native Enums and Case

Goal: compile tagged unions and pattern matching/case to native code.

Status: implemented. Enum construction now lowers to layout-driven native
storage with explicit tag writes and payload stores, `case` lowers to a
semantic `case` IR operation with checked tag dispatch and payload extraction,
native object backends emit enum tag/payload code for x86_64 ELF and arm64
Mach-O, enum test values serialize through semantic tests, and native-required
enum tests compare direct enum returns through the aggregate harness.

Deliverables:

```text
enum layout computation
variant construction lowering
case/match expression lowering
payload extraction
tag dispatch
exhaustiveness checking
variant identity preservation across rename
native enum test serialization
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/lowering.rs
src/backend/native.rs
src/migrations.rs
src/patch.rs
src/verify.rs
tests/native_enums.rs
```

Acceptance checks:

```text
Discount enum compiles native
case dispatch returns correct native result
payload extraction is layout-correct
non-exhaustive case is rejected unless default/else is explicit
renaming variant preserves semantic identity
trace maps case decision to semantic expression
```

## Phase 11 — Fixed Arrays and Array Places

Goal: support fixed-size aggregate storage before dynamic lists.

Status: implemented. Fixed arrays now have projection syntax, type checking,
deterministic layout metadata, array literals, array element places, native
index load/store lowering with bounds checks, record fields, and
layout-classified parameter/return support.

Deliverables:

```text
array<T, N> type
array literal
array layout
array index place
bounds check or statically proven bound
array fields in records
array parameters/returns by layout classification
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/layout.rs
src/lowering.rs
src/backend/native.rs
src/verify.rs
tests/fixed_arrays.rs
```

Acceptance checks:

```text
array<i64, 4> compiles native
array<Line, 4> compiles native
index load and index store lower to native address calculation
bounds trap maps to semantic expr_hash
verify validates array layout and index operation metadata
```

## Phase 12 — Slices and Reference-Containing Standard Types

Goal: prove references-in-records by implementing slices as native pointer+length views.

Status: implemented for array-backed `slice<'a, T>` and `mut_slice<'a, T>`, including projection syntax, `len`, index/index_mut, subslice, native pointer+length layout, dynamic bounds/range traps, tracing, and verification. Static bytes/string slice construction remains deferred until Phase 17 static data support.

Deliverables:

```text
slice<'a, T>
mut_slice<'a, T>
slice from array
slice from static bytes/string, when static data exists
len operation
index operation
index_mut operation
subslice operation
bounds-check lowering
slice projection syntax
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/layout.rs
src/lowering.rs
src/backend/native.rs
src/verify.rs
src/tests.rs
tests/slices.rs
```

Acceptance checks:

```text
slice<'a, Line> is represented as reference-containing native record
sum(slice<i64>) compiles native
mut_slice store compiles native and requires state effect
bounds failure traps natively and maps to semantic location
verify validates slice region and element layout
```

## Phase 13 — Loops and Folds

Goal: compile useful iteration over arrays and slices.

Status: implemented for the `fold item in target with acc = init do body`
expression over fixed arrays and slices. Fold lowering carries explicit loop
state, emits native loops on supported backends, traces loop iterations, and
validates accumulator/element types. `break` and `continue` remain reserved and
unsupported; general `for` syntax is deferred.

Deliverables:

```text
bounded for expression or statement
fold expression over fixed arrays/slices
loop-carried accumulator lowering
basic block control flow in lowered IR
break/continue only if explicitly scoped
trace loop iteration events
```

Files likely touched:

```text
src/expr.rs
src/types.rs
src/lowering.rs
src/backend/native.rs
src/trace.rs
src/debugger.rs
src/verify.rs
tests/loops.rs
examples/v2/invoice_static.cdb
```

Acceptance checks:

```text
invoice_total over slice/array compiles native
loop accumulator returns correct native result
loop body mutation requires state effect where applicable
trace/debug maps loop events to semantic expr_hash
verify validates control-flow and accumulator types
```

## Phase 14 — Invoice Static Acceptance

Goal: combine records, references, enums, arrays/slices, and loops into the first useful native v2 program.

Status: implemented. The `invoice_static.cdb` fixture now combines named
records, a reference-carrying `Invoice<'a>` record, `Discount` enum payloads,
fixed arrays, array-backed slices, shared borrows, and a fold-based invoice
total. The phase acceptance test creates a native-required invoice total test,
checks trace events for records, enums, references, slices, arrays, and loops,
checks the native build plan, verifies projection and history round trips, and
runs the native executable/test harness on supported hosts.

Deliverables:

```text
examples/v2/invoice_static.cdb
native-required tests for invoice totals
trace fixture
build fixture
verify fixture
replay/export/import fixture
```

Files likely touched:

```text
examples/v2/invoice_static.cdb
tests/v2_invoice_static.rs
docs/PLAN_V2.md
```

Acceptance checks:

```text
native build succeeds
native executable computes expected invoice total
native-required tests pass
trace includes record, enum, reference, slice/array, and loop events
verify passes after replay/export/import
```

## Phase 15 — Box and Owned Heap Values

Goal: support owned dynamic data without GC or interpreter fallback.

Deliverables:

```text
box<T> type
box_new operation
box dereference place
move-only ownership for boxes
drop glue for boxes
recursive type support through box
allocator interface
heap_alloc / heap_free lowered ops
minimal platform allocation externs
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/layout.rs
src/lowering.rs
src/backend/native.rs
src/verify.rs
src/tests.rs
new std/alloc or examples/std/alloc package
tests/box.rs
```

Acceptance checks:

```text
box<Line> compiles native
borrowing from box works
moving box prevents use-after-move
drop frees exactly once
recursive Node with option<box<Node>> typechecks and compiles native for basic construction/use
native-required box tests pass
```

Implementation note: Phase 15 is implemented. Allocation lowers through
`heap_alloc`; freeing is emitted by compiler-generated drop glue for box-owning
layouts, so there is no user-callable `heap_free` surface.

## Phase 16 — Raw Pointers, Unsafe, and FFI Boundary

Goal: expose low-level native interop without weakening safe semantic references.

Status: implemented. Raw pointer structural types are usable in projection and
apply surfaces, `unsafe` is a first-class effect, raw pointer conversion from
references and raw mutable-to-shared pointer casts lower through explicit
`ptr_cast`, raw load/store lower through `deref_raw` plus native load/store,
and raw load/store are limited to Copy, non-reference, trivially-droppable
pointees for the initial unsafe surface. Extern declarations with raw pointer
arguments or returns must declare both `ffi` and `unsafe`, platform externs
such as `write`, `malloc`, and `free` validate with explicit unsafe effects,
and compiled link plans list compiler-generated platform capsule relocations.

Deliverables:

```text
raw_ptr<T>
raw_mut_ptr<T>
unsafe effect or unsafe block marker
raw pointer conversions from references
raw load/store in unsafe context only
pointer casts, minimal
extern ABI validation with pointer args
platform capsule extern declarations
```

Files likely touched:

```text
src/types.rs
src/expr.rs
src/abi.rs
src/lowering.rs
src/backend/native.rs
src/verify.rs
src/api.rs
tests/raw_pointers.rs
tests/ffi.rs
```

Acceptance checks:

```text
safe code cannot dereference raw pointers
unsafe/ffi-marked function can pass raw pointer to extern
extern write/malloc/free declarations validate effects
raw pointer operations appear in trace/build diagnostics as unsafe/ffi
native link succeeds against platform capsule on supported hosts
```

## Phase 17 — Static Strings and Bytes

Goal: support native string/byte literals as read-only static data plus slice/string views.

Status: implemented for string and byte literal parsing/export, `StaticData`
objects, `slice<'static, u8>` views, `len`, native static data emission and
metadata, verifier coverage, and FFI write-wrapper usage. Equality/compare stays
deferred until a richer string abstraction needs it.

Deliverables:

```text
static byte object/artifact
string literal expression
bytes literal expression
read-only data section emission
string/bytes layout
len operation
equality or compare operation, if needed
stdout-ready byte/string view conversion
```

Files likely touched:

```text
src/expr.rs
src/types.rs
src/lowering.rs
src/backend/native.rs
src/artifact.rs
src/link.rs
src/verify.rs
tests/static_strings.rs
```

Acceptance checks:

```text
"hello" emits native read-only data
b"hello" emits native read-only data
slice/string length works natively
static string can be passed to std.io/std.platform write wrapper
verify validates static data artifacts and source maps
```

## Phase 18 — Minimal Platform Capsule and Compiled Standard Library Skeleton

Goal: avoid a fat runtime by compiling stdlib logic as CodeDB code and limiting native capsule functions.

Deliverables:

```text
std.core package
std.mem package
std.platform package with minimal externs
std.io wrapper over platform write/read
std.alloc wrapper over platform malloc/free
build plan reporting of platform externs
capability metadata for stdlib calls
```

Initial externs:

```text
write
read, optional at first
malloc
free
trap
exit
```

Files likely touched:

```text
std/core.cdb or examples/std/core.cdb
std/mem.cdb
std/platform/<target>.cdb
std/io.cdb
std/alloc.cdb
src/build_plan.rs
src/link.rs
src/abi.rs
tests/stdlib.rs
```

Acceptance checks:

```text
stdlib functions compile as CodeDB semantic programs where practical
platform capsule is visible in build plan
no semantic interpreter is linked
hello/stdout program compiles native and prints expected output
allocation wrappers compile and link where supported
```

## Phase 19 — Args, Stdout, and Simple CLI Native Program

Goal: compile a small useful CLI-style program with minimal I/O.

Deliverables:

```text
process entry metadata
args capability, if target support is ready
stdout capability
exit code behavior
native CLI integration test harness
```

Files likely touched:

```text
src/model.rs
src/abi.rs
src/link.rs
src/backend/native.rs
src/main.rs
std/io.cdb
std/platform/<target>.cdb
tests/native_cli.rs
examples/v2/hello_invoice.cdb
```

Acceptance checks:

```text
native executable prints expected output
build plan lists stdout capability
native-required CLI test captures stdout and exit code
no interpreter/runtime dispatcher participates
```

## Phase 20 — Dynamic Buffers, Vec, and String

Goal: build dynamic data structures on top of box/allocator/raw-pointer platform boundary.

Deliverables:

```text
owned buffer representation
vec<T> or dynamic_array<T>
string owned representation
push/get/len operations
capacity management in CodeDB stdlib where possible
drop glue for buffers
bounds checking
```

Files likely touched:

```text
std/alloc.cdb
std/vec.cdb
std/string.cdb
src/types.rs
src/expr.rs
src/lowering.rs
src/backend/native.rs
src/verify.rs
tests/dynamic_buffers.rs
```

Acceptance checks:

```text
vec<i64> append/read compiles native
owned string construction compiles native
drop frees buffers exactly once
stdlib implementation is mostly CodeDB code
native-required tests pass
```

## Phase 21 — Parser or Word Count Acceptance

Goal: compile a useful native text-processing program.

Deliverables:

```text
examples/v2/word_count.cdb or examples/v2/parser.cdb
native-required tests
string/byte slice processing
loop fixtures
bounds trap fixture
trace/debug fixture
```

Files likely touched:

```text
examples/v2/word_count.cdb
tests/v2_word_count.rs
std/string.cdb
std/io.cdb
```

Acceptance checks:

```text
native executable processes byte/string input correctly
bounds checks are native and semantic
trace maps loop and slice operations to semantic expr_hashes
verify passes after replay/export/import
```

## Phase 22 — File I/O and Stateful CLI Capstone

Goal: compile a useful stateful native tool.

Deliverables:

```text
read_file capability
write_file capability
result/error handling helpers
file-content buffers
stdlib file wrappers over minimal platform externs
todo_cli acceptance app or equivalent
```

Files likely touched:

```text
std/io.cdb
std/result.cdb
std/string.cdb
examples/v2/todo_cli.cdb
tests/v2_todo_cli.rs
src/build_plan.rs
src/link.rs
src/verify.rs
```

Acceptance checks:

```text
native executable reads and writes test files
build plan lists file capabilities
native-required integration tests pass in temp sandbox
stdlib logic is CodeDB-compiled where practical
verify validates effects and platform extern requirements
```

## Phase 23 — V2 Semantic Patch and Agent Operations

Goal: expose new v2 program-model changes as intent-preserving semantic operations.

Deliverables:

```text
extract_record
rename_field
add_field_with_default
remove_field_and_update_constructors
rename_variant_and_cases
borrow_parameter
convert_by_value_param_to_ref
thread_mut_cursor
extract_slice_view
introduce_box
replace_raw_pointer_with_safe_reference, where possible
```

Files likely touched:

```text
src/patch.rs
src/migrations.rs
src/api.rs
src/workspace.rs
src/verify.rs
tests/v2_patch.rs
```

Acceptance checks:

```text
patch preview shows planned region/borrow/layout impact
patch apply is expected-root safe
failed patch leaves branch unchanged
rename_field preserves field identity
convert_by_value_param_to_ref updates callers and native tests pass
build impact distinguishes metadata/layout/codegen changes
```

## Phase 24 — V2 Provenance, Blame, and Why

Goal: make memory/layout/native decisions explainable.

Deliverables:

```text
blame-type
blame-field
blame-variant
why-borrow
why-move
why-drop
why-layout
why-effect
why-platform-extern
```

Files likely touched:

```text
src/provenance.rs
src/verify.rs
src/layout.rs
src/workspace.rs
src/api.rs
src/main.rs
tests/v2_provenance.rs
```

Acceptance checks:

```text
can explain why a field offset exists
can explain why a mutable borrow conflicts
can explain why a function requires state/alloc/io
can explain why a platform extern appears in link plan
outputs cite semantic object and migration hashes
```

## Cross-cutting verification work

Every phase after Phase 3 should update verification.

Required v2 verification themes:

```text
type definitions and region params
layout determinism
reference validity
borrow exclusivity
move-only correctness
drop correctness
effect coverage
unsafe boundary containment
native lowered IR well-formedness
platform capsule declarations
artifact cache key completeness
```

Acceptance for each phase:

```text
verify catches at least one malformed object for the new feature
verify passes valid acceptance fixtures
replay/export/import preserve validity
```

## Cross-cutting native backend policy

The backend may be conservative.

Allowed conservative choices:

```text
pass large aggregates indirectly
return large aggregates through hidden slots
use stack slots instead of register allocation where simpler
emit simple bounds checks
emit simple drop glue
link small platform capsule objects
```

Not allowed:

```text
fall back to interpreter execution
hide language semantics in opaque host calls
claim feature completion without native-required tests
skip verification for memory/layout features
```

## Suggested milestone cuts

### Milestone V2.1 — References in Records Native

Includes:

```text
TypeDef/RecordDef
regions
layout model
place IR
shared refs
mutable refs
references in records
copy/move basics
native records
line_view_refs
mutable_cursor
```

Success:

```text
records containing references compile native and pass native-required tests
```

### Milestone V2.2 — Structured Native Programs

Includes:

```text
enums/case
fixed arrays
slices
loops/folds
invoice_static
```

Success:

```text
invoice_static compiles to a native executable and passes native-required tests
```

### Milestone V2.3 — Owned Memory and Unsafe Boundary

Includes:

```text
box<T>
drop glue
allocator interface
raw pointers
minimal platform capsule
static strings/bytes
```

Success:

```text
owned heap values and static strings compile native without interpreter fallback
```

### Milestone V2.4 — Compiled Standard Library and CLI

Includes:

```text
std.core
std.mem
std.alloc
std.string
std.io
stdout/args/file capabilities
word_count or parser
todo_cli capstone
```

Success:

```text
a useful CLI tool compiles native, uses CodeDB stdlib, passes native integration tests, and verifies effects/capabilities
```

## Risks and mitigations

### Risk: lifetimes become too complex

Mitigation:

```text
start with explicit region parameters
limit inference
reject ambiguous programs
avoid self-referential movable records initially
add arenas/pin later
```

### Risk: native ABI complexity delays all language work

Mitigation:

```text
use conservative indirect passing for aggregates
prioritize correctness over register ABI optimality
make layout metadata explicit and verified
```

### Risk: support library grows into a runtime

Mitigation:

```text
keep platform capsule tiny
compile stdlib logic as CodeDB code
make build plan list every platform extern
forbid semantic interpreter fallback in native-required tests
```

### Risk: unsafe/raw pointers undermine semantic guarantees

Mitigation:

```text
make raw dereference unsafe/ffi-only
prefer safe refs/slices/boxes in stdlib APIs
verify unsafe boundary markers
surface unsafe effects in patches/build plans
```

### Risk: drop/move bugs become native memory bugs

Mitigation:

```text
start with simple move-only rules
generate explicit drop paths
verify drops exactly once
native tests exercise move/drop failures
avoid implicit copies of move-only values
```

## Out of scope for initial v2

Initial v2 should not require:

```text
GC
self-referential movable records
full lifetime elision/inference
async/concurrency
high-performance optimizer
full C ABI coverage
full DWARF debug info
package registry
cross-machine distributed object exchange beyond existing import/export
```

These can follow after the native semantic memory and useful-program foundations are solid.
