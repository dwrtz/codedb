# SPEC_V2.md — CodeDB Native Semantic Programming Model

Status: Draft 1.0
Scope: v2 design track built on the v1 semantic workspace

## 1. Thesis

CodeDB v2 turns the v1 semantic workspace into a native semantic programming system for useful programs.

A program is still not primarily a collection of files. A program is an immutable, content-addressed semantic DAG plus replayable migration history, inspected and changed through root-aware semantic operations. V2 extends that DAG so it can model real program memory, richer data, mutation, ownership, references, effects, and native compilation.

The central v2 rule is:

```text
No language or program-model feature is considered done until it compiles to native artifacts.
```

The reference evaluator may remain useful as a semantic oracle for tests, trace comparison, and debugging. It is not a v2 completion backend. Generated projections may remain useful for review and debugging. They are not the source of truth.

V2's central question is:

```text
Can CodeDB model useful programs semantically and compile them to native executables without falling back to a semantic interpreter?
```

## 2. Relationship to v1

V1 makes CodeDB usable as a semantic workspace. V2 keeps that workspace model and expands what the semantic program can express and compile.

V1 asks:

```text
Can humans and agents use semantic roots, migrations, branches, traces, tests, patches, provenance, and artifacts as a practical workspace?
```

V2 asks:

```text
Can that workspace represent and compile useful native programs with structured data, semantic memory, references, effects, and a standard library?
```

V2 should not replace these v1 concepts:

- immutable content-addressed objects;
- deterministic hashing;
- roots and branches;
- stable symbol identity;
- names as metadata;
- structural migrations;
- replayable histories;
- root-aware transactions;
- artifact caches and jobs;
- semantic tests;
- semantic trace/debug identities;
- patches and provenance;
- verification.

V2 should extend them so program modeling and compilation are no longer limited to small scalar examples.

## 3. Design principles

1. Semantic objects remain the source of truth.
2. Native compilation is mandatory for v2 feature completion.
3. The reference evaluator is an oracle, not a completion backend.
4. Projection text is a view, not authoritative source.
5. Machine addresses are not semantic truth.
6. Semantic memory is modeled as places, regions, references, loans, ownership, moves, drops, and effects.
7. References in records are a core v2 feature.
8. Safe references are normal; raw pointers are restricted to unsafe and FFI boundaries.
9. Lifetimes/regions are semantic and verified; they are erased during native code generation.
10. Mutable access must be exclusive and root-verifiable.
11. Values must have deterministic target layouts before they are native-lowerable.
12. Copy, move, drop, and allocation behavior must be explicit enough for verification.
13. Effects and capabilities must be visible in signatures, tests, traces, and build plans.
14. The standard library should be ordinary CodeDB code wherever possible.
15. Platform capsules should be minimal native boundaries, not hidden runtimes.
16. Verification remains mandatory.

## 4. Native completion rule

A v2 feature is complete only when all relevant layers support it:

```text
semantic object payloads
canonical hashing
object edges
root/index integration
projection syntax
structural apply operations
semantic patch support for features exposed through the semantic patch language
type checking
region / borrow / move / drop checking where applicable
effect checking where applicable
reference evaluation as oracle where applicable
semantic tests
native-required tests
trace/debug semantic locations
lowered IR
native object backend
ABI/layout classification
link/executable path
artifact cache keys
replay/export/import
verification
```

A feature that works only in projection text or only in the reference evaluator is not done for v2.

## 5. Native execution model

V2 produces native artifacts from semantic program objects.

Allowed:

```text
CodeDB semantic program -> lowered IR -> native object artifact -> link plan -> executable
CodeDB semantic program -> compiled CodeDB standard library -> native object artifacts
small platform capsule objects or externs for OS/ABI boundaries
reference evaluator for oracle/debug/test comparison
```

Not allowed as a completion path:

```text
semantic DAG -> interpreter -> result
semantic DAG -> hidden runtime dispatcher -> result
projection text as authoritative source
native feature "support" implemented only by opaque host calls
```

A platform capsule may provide primitive target services, such as process entry, allocation, deallocation, writing to a file descriptor, reading from a file descriptor, or trapping. It must not execute CodeDB semantic objects as an interpreter.

## 6. Standard library and platform capsule

V2 should prefer this layering:

```text
user CodeDB program
  -> CodeDB standard library packages
    -> tiny platform capsule / extern boundary
      -> OS, libc, or target syscalls
```

The compiler should know how to compile memory, references, layouts, calls, returns, traps, and ABI boundaries. Higher-level services should be CodeDB packages.

Suggested library split:

```text
std.core
  option, result, comparisons, small helpers

std.mem
  references, slices, arrays, copy/move helpers exposed safely

std.alloc
  box, owned buffers, dynamic vectors, allocator interfaces

std.string
  string views, byte slices, parsing/formatting helpers

std.io
  args, stdout, stderr, files, process exit wrappers

std.platform
  minimal extern declarations for the active target
```

The platform capsule should be as small as practical. Initial capsule candidates:

```text
extern write(fd: i64, ptr: raw_ptr<u8>, len: i64) -> i64 effects [ffi, io]
extern read(fd: i64, ptr: raw_mut_ptr<u8>, len: i64) -> i64 effects [ffi, io]
extern malloc(size: i64, align: i64) -> raw_mut_ptr<u8> effects [ffi, alloc]
extern free(ptr: raw_mut_ptr<u8>) -> unit effects [ffi, alloc]
extern trap(code: i64) -> unit effects [ffi, trap]
extern exit(code: i64) -> unit effects [ffi]
```

These are platform boundaries, not a runtime interpreter.

## 7. Type definitions and stable data identity

V2 should introduce named program-model objects beyond function definitions.

Required objects:

```text
TypeDef
  stable symbol identity for named records, enums, and aliases

RecordDef
  stable field identities
  ordered native layout fields
  region parameters
  type parameters when generics arrive

EnumDef
  stable variant identities
  payload types
  tag/layout policy
  region parameters

ConstantDef
  named immutable values where useful

EntryPoint
  native executable/test/benchmark entry metadata

CapabilityDef
  declared effects/capabilities required by an entry or package

PackageManifest / ModuleManifest
  reusable semantic library boundary
```

Names remain metadata. Renaming a type, field, or variant should preserve stable semantic identity where the underlying concept is the same.

## 8. Target-independent layout model

Before a value can compile to native code, CodeDB must know its target layout.

A layout result should be deterministic for:

```text
type_hash
target_triple
layout_version
abi_version
```

Suggested layout artifact:

```json
{
  "schema": "codedb/type-layout/v2",
  "type_hash": "sha256:...",
  "target_triple": "x86_64-unknown-linux-gnu",
  "layout_version": "layout:v2",
  "kind": "record",
  "size_bytes": 16,
  "align_bytes": 8,
  "copy_kind": "copy",
  "drop_kind": "trivial",
  "abi": {
    "pass": "by_value",
    "return": "by_value"
  },
  "fields": [
    {
      "field_symbol": "sha256:...",
      "name": "price_cents",
      "type_hash": "sha256:type-i64",
      "offset_bytes": 0,
      "size_bytes": 8,
      "align_bytes": 8
    }
  ]
}
```

Required layout kinds:

```text
scalar
record
enum
fixed_array
reference
box
slice
raw_pointer
static_string_or_bytes_view
```

Required classifications:

```text
copy
move_only
needs_drop
contains_reference
contains_mut_reference
contains_raw_pointer
contains_box
contains_capability_handle
```

Verify must recompute layouts and reject malformed metadata.

## 9. Semantic memory model

V2 introduces semantic memory.

Semantic memory is not arbitrary address arithmetic. It is a verified model of storage and access.

Core concepts:

```text
Value
  computed data

Place
  a storage location that can be read, assigned, borrowed, or moved from

Address
  lowered native pointer derived from a place

Loan
  a shared or mutable borrow of a place for a region

Region
  a semantic lifetime parameter or inferred local lifetime

Owner
  storage responsible for dropping/freeing a move-only value
```

Place kinds:

```text
local
parameter
record field
array element
slice element
box dereference
reference dereference
static item
raw pointer dereference, unsafe only
```

Machine pointers are lowering artifacts. Semantic traces and verification should speak in terms of places, fields, regions, and expression hashes, not unstable process addresses.

## 10. References and regions

Reference types carry semantic regions:

```text
&'a T
&'a mut T
```

Where `'a` is a region parameter or inferred local region.

Shared references permit reads. Mutable references permit reads and writes but require exclusive access.

Initial borrow rules:

```text
Many shared loans to the same place may coexist.
One mutable loan to a place may exist at a time.
A mutable loan conflicts with shared loans of the same place.
A loan may not outlive the storage it points into.
A function may not return a reference to one of its own locals.
A record containing a reference extends the loan for as long as that record value is live.
Moving a reference-containing record moves its loans.
Dropping a reference-containing record ends its loans.
```

Function signatures may be region-polymorphic:

```text
fn first_line<'a>(invoice: &'a Invoice) -> &'a Line
```

Type definitions may be region-polymorphic:

```text
record LineView<'a> {
  line: &'a Line
}

record Parser<'src> {
  input: slice<'src, u8>
  pos: i64
}

record MutCursor<'a> {
  buffer: &'a mut Buffer
  pos: i64
}
```

Human region names are projection metadata. Hashing should use canonical region parameter identities.

## 11. References in records

References in records are required for v2.

They are necessary for:

```text
slices
string views
parsers
cursors
iterators
borrowed contexts
views over large data
mutable editing handles
efficient APIs that avoid copying
```

A record field may have type:

```text
&'a T
&'a mut T
slice<'a, T>
mut_slice<'a, T>
raw_ptr<T>, unsafe/FFI only
raw_mut_ptr<T>, unsafe/FFI only
```

Example:

```text
record Line {
  price_cents: i64
  qty: i64
}

record LineView<'a> {
  line: &'a Line
}

fn line_total(view: LineView) -> i64 =
  view.line.price_cents * view.line.qty
```

Mutable example:

```text
record LineEditor<'a> {
  line: &'a mut Line
}

fn add_fee(editor: LineEditor, fee: i64) -> unit effects [state] =
  editor.line.price_cents = editor.line.price_cents + fee
```

The compiler must reject duplicated mutable reference records that alias the same place.

Self-referential movable records are not part of the initial v2 requirement:

```text
record SelfRef {
  data: Buffer
  view: slice<'self, u8>
}
```

Such values require a stable-address discipline such as `pin`, arenas, or explicit immovable allocation. They are a later extension.

## 12. Copy, move, drop, and ownership

References in records require value classification.

Initial rules:

```text
i64, bool, unit are Copy.
Shared references are Copy.
Mutable references are move-only.
Raw pointers are Copy but unsafe to dereference.
box<T> is move-only.
A record is Copy if all fields are Copy.
A record is move-only if any field is move-only.
A record needs drop if any field needs drop.
An enum is classified from all payload variants.
Moving a value invalidates the old place unless the value is Copy.
Using a moved value is rejected.
Dropping a value recursively drops owned fields and ends loans.
```

This classification must be part of type checking, lowering, native codegen, and verification.

## 13. Owned heap values

V2 should support owned heap pointers after references and layouts are stable.

User-facing type:

```text
box<T>
```

Semantics:

```text
box<T> owns a heap allocation containing T.
Moving a box transfers ownership.
Dropping a box drops T and frees the allocation.
Borrowing from a box is allowed.
A box may be stored in records and enum variants.
Recursive types are represented through box or future arena references.
```

Example:

```text
record Node {
  value: i64
  next: option<box<Node>>
}
```

Lowering:

```text
box_new(value) -> allocation + store
box dereference -> address of owned payload
drop(box) -> drop payload + free allocation
```

Allocation uses the minimal platform capsule or an allocator package compiled from CodeDB plus platform externs.

## 14. Slices and string/byte views

Slices are reference-containing records with special compiler support where needed.

Suggested semantic types:

```text
slice<'a, T>
mut_slice<'a, T>
```

Native representation:

```text
{ ptr: *T, len: i64 }
```

Safe operations:

```text
len(slice)
index(slice, i)
index_mut(mut_slice, i)
subslice(slice, start, len)
```

Indexing either proves bounds statically or lowers to a bounds check and semantic trap.

Static strings and bytes can lower to read-only data plus slice views:

```text
"hello" -> slice<'static, u8> or string<'static>
b"hello" -> slice<'static, u8>
```

Dynamic strings and vectors should build on owned buffers after `box`, allocator interfaces, and drop are implemented.

## 15. Raw pointers and unsafe boundaries

Safe CodeDB programs should not manipulate arbitrary addresses.

Raw pointers exist for low-level and FFI work:

```text
raw_ptr<T>
raw_mut_ptr<T>
```

Rules:

```text
Raw pointer values may be passed around as data.
Raw pointer dereference requires unsafe or ffi context.
Pointer/integer casts require unsafe.
Pointer arithmetic requires unsafe and should be minimal.
Safe references may be converted to raw pointers at explicit unsafe/FFI boundaries.
Raw pointers do not carry region guarantees.
```

Consider adding an explicit effect:

```text
unsafe
```

If not added, unsafe operations must at least be represented distinctly from ordinary `ffi`.

## 16. Effects and capabilities

Effects should be enforced, not merely documented.

Relevant effects:

```text
pure
trap
state
alloc
io
ffi
unsafe
concurrent, later
```

Rules:

```text
Writing through &mut T requires state. More generally, any assignment to a
semantic place requires state: v2 has no immutable-binding distinction yet, so
every local is mutable and every assignment is treated as a state effect. This
is conservative (it can over-require state on a function that only mutates a
private local), which is safe because effect checking fails closed; a future
let-mut / immutability distinction may narrow it back toward "only writes
through &mut T".
Allocating or freeing owned heap values requires alloc, unless hidden as compiler-generated drop under a declared allocation model.
Bounds checks may trap.
Raw pointer dereference requires unsafe.
Extern calls require ffi and any effects declared by the extern.
I/O functions require io.
Pure functions may not call effectful functions unless effect polymorphism is explicitly supported.
```

Capabilities should be visible in entry-point and build metadata:

```text
args
stdout
stderr
read_file
write_file
env
clock
network, later
```

The build plan should be able to answer:

```text
What capabilities does this program require?
What platform externs will be linked?
What new effects does this patch introduce?
```

## 17. Lowered IR requirements

V2 lowered IR must become memory-aware.

Required concepts:

```text
value ids
place ids
address ids
basic blocks
control-flow terminators
layout metadata references
drop paths
borrow/debug metadata
```

Required operations:

```text
alloca
addr_of_local
addr_of_param
addr_of_field
addr_of_index
addr_of_deref
load
store
copy
move
drop
borrow_shared
borrow_mut
borrow_end
construct_record
construct_enum
extract_field
case_switch
heap_alloc
heap_free
bounds_check
ptr_cast, unsafe only
call
return
trap
```

Some borrow operations may not emit machine instructions, but they should appear as verification/debug metadata.

## 18. Native backend requirements

The native backend must support:

```text
record layout and field offsets
enum tag/payload layout
reference fields as non-null pointer fields
mutable reference fields as exclusive pointer fields at the semantic layer
fixed arrays
slices as pointer+length values
load/store through references
assignment through mutable references
function calls with by-value and by-reference ABI classification
hidden return slots where required
stack slots for addressable locals
heap allocation calls for box<T>
drop glue generation
bounds-check traps
raw pointer extern calls
```

The backend may choose conservative ABI lowering before optimizing:

```text
large aggregates passed indirectly
large aggregate returns use hidden return slots
small scalars passed in target registers where implemented
records/enums may initially use stack memory more often than necessary
```

Correctness and deterministic artifacts come before optimal code generation.

## 19. Tests, trace, and debug

V2 tests should support native-required mode.

Suggested test mode:

```json
{
  "mode": "reference_and_native",
  "native_required": true
}
```

If native codegen is unavailable or unsupported for a feature, a native-required test fails.

Trace/debug should use semantic identities:

```text
root_hash
symbol_hash
function_def_hash
expr_hash
place path
region id
loan id
lowered op id
```

Trace/debug should not treat process addresses as stable semantic identities.

Example trace event:

```json
{
  "event": "borrow_mut",
  "frame": 2,
  "expr_hash": "sha256:...",
  "place": {
    "root": "local",
    "name": "line",
    "path": ["price_cents"]
  },
  "region": "r0",
  "type_hash": "sha256:..."
}
```

Native debug metadata may map machine addresses back to lowered operations and semantic places.

## 20. Verification

V2 verification must cover semantic memory and native layout.

Required checks:

```text
all type references resolve
all region references resolve
record fields have stable identities and valid types
enum variants have stable identities and valid payload types
layouts recompute deterministically
copy/move/drop classification recomputes deterministically
borrow rules hold
references do not escape their storage
reference-containing records carry valid loans
mutable loans are exclusive
move-only values are not copied
moved values are not used
drops occur exactly once for owned values
raw pointer operations are unsafe/ffi-marked
function effects cover body effects
native lowered IR is valid for the semantic types
source maps/debug maps point to reachable semantic objects
artifact cache keys include all relevant layout/backend versions
```

Verification should fail closed. If CodeDB cannot prove a memory or native-layout property, the program is not valid v2 native input.

## 21. Acceptance programs

V2 should be accepted through native-compiled programs, not evaluator-only demos.

### 21.1 Line view references

Demonstrates references in records.

```text
record Line {
  price_cents: i64
  qty: i64
}

record LineView<'a> {
  line: &'a Line
}

fn line_total(view: LineView) -> i64 =
  view.line.price_cents * view.line.qty
```

Acceptance:

```text
native build succeeds
native-required test passes
trace maps reference field access to semantic place
verify validates region and borrow rules
```

### 21.2 Mutable cursor

Demonstrates mutable references in records and state effects.

```text
record LineEditor<'a> {
  line: &'a mut Line
}

fn add_fee(editor: LineEditor, fee: i64) -> unit effects [state] =
  editor.line.price_cents = editor.line.price_cents + fee
```

Acceptance:

```text
native build succeeds
mutation changes native result
borrow checker rejects duplicate mutable editors for the same place
verify validates exclusive loan behavior
```

### 21.3 Invoice static

Demonstrates records, enums, fixed arrays, references, and loops/folds.

```text
record Money { cents: i64 }
record Line { price: Money, qty: i64 }
record Invoice<'a> { lines: slice<'a, Line> }

enum Discount {
  None: unit
  Percent: i64
  Fixed: Money
}
```

Acceptance:

```text
native executable computes invoice total
native-required semantic tests pass
trace/debug identifies loop, case, field, and reference operations
verify validates layout, regions, and effects
```

### 21.4 Parser or word-count

Demonstrates slices, static/dynamic bytes, loops, and later I/O.

Acceptance:

```text
native executable processes byte/string input
bounds checks are native and map to semantic traps
stdlib helpers compile as CodeDB code
platform capsule remains minimal
```

### 21.5 Stateful CLI tool, later v2 capstone

Demonstrates args, stdout, file I/O, strings, dynamic allocation, owned buffers, and result/error handling.

Acceptance:

```text
native executable performs useful CLI task
all standard-library logic is CodeDB-compiled where practical
platform extern usage is visible in build plan
native-required integration tests pass
```

## 22. Non-goals for initial v2

Initial v2 should not require:

```text
GC
JIT execution
semantic interpreter fallback
arbitrary C pointer arithmetic in safe code
self-referential movable records
full Rust-compatible lifetime inference
async/concurrency
full optimizing compiler
full DWARF support
package registry or distributed dependency resolution
```

Some of these may arrive later. They are not required before CodeDB can compile useful native programs.

## 23. Open design questions

Questions to settle during v2 implementation:

```text
Should unsafe be a first-class effect, a block marker, or both?
How much region inference should v2 attempt versus requiring explicit region parameters?
Should large records be passed indirectly by default for all targets?
Should slices be primitive types or standard-library records with compiler-known layout?
What is the first allocator contract for box<T>?
How should drop glue be represented as semantic objects or compiler-generated artifacts?
How should native test results serialize structured values?
What minimal platform capsule is acceptable for Linux and Apple targets?
```

The implementation plan should answer these incrementally through native acceptance programs.
