# PLAN_V3.md — CodeDB Self-Hosting Semantic Programming Roadmap

Status: Draft 1.0
Scope: implementation roadmap for the v3 self-hosting track ([SPEC_V3.md](SPEC_V3.md))

## Direction

V3 turns the v2 native compiler into a self-hosting one. The compilation
pipeline is expressed as CodeDB objects, built concurrently by agents through
structural operations, and checked against the trusted Rust compiler at every
seam.

The v3 target workflow is:

```text
express a compiler stage as semantic objects
  -> apply structural operations / patches (concurrently, via the agent spine)
  -> verify types, regions, borrows, moves, effects, layouts, and new identities
  -> lower to memory-aware IR
  -> compile to native object artifacts
  -> run the self-hosted stage
  -> compare its artifact to the Rust stage's under the determinism oracle
  -> verify replay and artifacts
```

Two implementation rules govern v3:

```text
No runtime/interpreter fallback for feature completion.  (inherited from v2)
A pipeline stage is self-hosted only when the CodeDB stage reproduces the Rust
  stage's artifact, hash-for-hash or byte-for-byte, and passes verify and replay.
```

Self-hosting is staged at the lowered-IR seam. A mixed compiler — CodeDB
front-end feeding the Rust native backend at the IR boundary — is a legitimate
interim. Projections (`emit-c`, source text) are views, never a compilation
path; self-hosting may not route through them.

## Current implementation baseline

V3 builds on the complete v2 native compiler:

```text
Rust CLI and library crate, SQLite-backed immutable object store
canonical JSON hashing; roots, branches, stable symbol identity
interface_hash / implementation_hash incremental firewall
TypeDef/RecordDef/EnumDef; region-parameterized records and functions
borrow/move/drop checker; copy/move/drop classification from layout
records, enums + case, fixed arrays, slices, fold loops — all native
box<T> owned heap + whole-slot drop glue; raw pointers / unsafe / FFI
compiled std.core/mem/alloc/string/io over a tiny platform capsule
stdout/exit/file I/O; dynamic vec<T> + owned string
semantic patches; provenance / why surface
native object backends for x86_64 ELF and arm64 Mach-O; link plans; verify
the Rust compiler itself, retained as trusted stage-0 and the determinism oracle
```

V3 must preserve every v2 demo, test, and invariant while expanding the language
and self-hosting the pipeline.

## Native-done definition

V3 carries two layered definitions of done.

The **native completion rule** (inherited) applies to every new *language
feature*. A feature is done only when payloads, canonical hashing, edges,
projection syntax, structural apply, patch support where exposed, type checking,
region/borrow/move/drop checking, effect checking, reference-evaluator oracle,
trace/debug locations, lowered IR, native object backend, ABI/layout,
native-required tests, verify, and replay/export/import all support it.

The **self-hosting completion rule** (new) applies to every *pipeline stage*:

```text
the stage is expressed as CodeDB semantic objects
the stage obeys the native completion rule
the stage's output matches the Rust stage's artifact under the determinism oracle
verify validates the self-hosted stage's objects
replay/export/import round-trips the self-hosted stage
```

A stage correct only in the reference evaluator is not self-hosted. A feature
the self-hosted compiler needs but that is not native-complete is not done.

## Acceptance programs

V3 is driven by the self-hosting ladder rungs and the forcing-function programs
that drive the expressiveness floor. Ladder rungs live under a new
`compiler/` tree of `.cdb` objects; forcing programs live under `examples/v3/`.

```text
ladder rungs (each with a determinism oracle):
  rung 0  reference evaluator in CodeDB     oracle: result == Rust evaluator
  rung A  front-end -> lowered IR           oracle: IR-hash == Rust front-end
  rung B  native object emission (.o)       oracle: bytes == Rust emitter
  rung C  link plan                         oracle: JSON == Rust linker driver

forcing-function programs:
  examples/v3/tokenizer.cdb   forces early exit (R7) and bytes->int (R6)
  examples/v3/sha256.cdb      forces bitwise/sized/casts (R4/R5/R6); validates
                              the hashing rung A's importer depends on
```

Each acceptance program should carry the v2 fixture set: source projection,
apply JSON where useful, native-required tests, trace/debug, verify, and
replay/export/import.

## Phase 1 — Version Boundary and Self-Hosting Docs

Goal: establish v3 as the self-hosting track and document the two completion rules.

Status: implemented. SPEC_V3.md, SPEC_V4.md, PLAN_V3.md, the README documentation
map, examples/v3/README.md, and the compiler/ object-tree skeleton are in place;
the self-hosting completion rule and determinism oracle are documented.

Deliverables:

```text
create docs/SPEC_V3.md                               (done)
create docs/SPEC_V4.md                               (done — horizon sketch)
create docs/PLAN_V3.md                               (done — this document)
update README documentation map                      (done)
add examples/v3/README.md and the compiler/ skeleton (done)
document the self-hosting completion rule and the oracle (done)
```

Files likely touched:

```text
docs/SPEC_V3.md, docs/SPEC_V4.md, docs/PLAN_V3.md
README.md, examples/v3/README.md
```

Acceptance checks:

```text
README points to v0/v1/v2/v3 docs and the v4 sketch
v3 docs forbid interpreter fallback and forbid compiling through projections
the self-hosting ladder and its oracles are named
existing tests still pass; no command behavior changes in this phase
```

## Phase 2 — Architecture Paydown: Feature Registry and Oracle Conformance

Goal: shrink the per-feature edit surface and pin the reference evaluator to the
native backend, so the many feature phases that follow are cheap to build and
safe to build concurrently.

Status: implemented. `src/op_registry.rs` is the single source of truth for
built-in operators (one `OPS` row per operator); the evaluator (`expr.rs`), the
lowering source-op→kind / verify / trap mappings (`lowering.rs`), and the parser
precedence table all forward to it, collapsing the per-operator surface from six
sites to one (the backend's machine-code encoders are the irreducible second
site, guarded by a registry-driven coverage test). `src/oracle.rs` provides the
determinism-oracle helper (hash / bytes / canonical-JSON identity) for every
ladder rung, and `tests/oracle_conformance.rs` drives every operator through both
the reference evaluator and the native backend, with a coverage gate over
`operator_kinds()` so a new operator without a fixture fails loudly. The refactor
is output-preserving; the full existing suite stays green.

Rationale: today a language feature touches `migrations.rs` (Operation),
`patch.rs`, `verify.rs`, `provenance.rs` (blame), the evaluator in `expr.rs`, and
`backend/native.rs` as six separate edits, and the evaluator and native backend
are two divergent consumers of the lowered IR. At v3 feature volume this tax
compounds and creates agent merge conflicts.

Deliverables:

```text
a single source of truth for expression forms / operations that derives the
  verify, patch, and provenance scaffolding (collapse the 6-site edit toward 1-2)
an evaluator-vs-backend conformance harness that asserts, per lowered-IR op,
  that the reference evaluator and the native backend agree (oracle stays honest)
a determinism-oracle helper usable by every ladder rung
```

Files likely touched:

```text
src/expr.rs, src/lowering.rs, src/backend/native.rs
src/verify.rs, src/patch.rs, src/provenance.rs, src/migrations.rs
tests/oracle_conformance.rs
```

Acceptance checks:

```text
adding a trivial operator touches <= 2 source sites end to end
the conformance harness fails if the evaluator and backend disagree on any op
existing native-required tests still pass
```

## Phase 3 — Minimum Agent Spine: Semantic Merge and Proof-Carrying Receipts

Goal: establish, early, the editing layer that lets several agents build the
compiler-in-CodeDB concurrently without falling back to projection text.

Status: implemented. The semantic-merge substrate (common-ancestor root,
migration replay, semantic conflict detection, build-impact recomputation) was
already present and is now joined by a hash-pruned expression tree diff
(`diff_exprs_json`, exact because node hashes are Merkle hashes). Every
structural write now returns, pre-commit, a proof-carrying `MigrationReceipt`
(emitted under the summary's `receipt` key): typecheck verdict, borrow-check
invariant, per-symbol effect delta and root capability-surface delta,
build-impact verdict, and the hash-pruned semantic diff — flowing through CLI
apply, `ops.apply`/`ops.preview`, and merge apply. Merge reports its recomputed
build impact, consistent with the receipt. `tests/agent_concurrency.rs` proves N
`--expect-root` writers serialize to exactly one applied (no lost updates) and
that N identical (same request_id) submissions replay one committed response.
Sized to "a few agents build one compiler"; the full agent-native platform is v4
([SPEC_V4.md](SPEC_V4.md)).

Deliverables:

```text
semantic merge: common-ancestor root + migration replay + semantic conflict
  detection + hash-pruned tree diff + build-impact recomputation
proof-carrying receipts on structural writes: typecheck summary, borrow/effect/
  capability delta, build-impact verdict, and a semantic diff, returned pre-commit
multi-agent optimistic concurrency hardening over the existing --expect-root path
```

Files likely touched:

```text
src/merge.rs, src/diff.rs, src/build_plan.rs
src/workspace.rs, src/api.rs, src/provenance.rs
tests/semantic_merge.rs, tests/agent_concurrency.rs
```

Acceptance checks:

```text
two branches with disjoint structural edits merge with no conflict and a correct
  recomputed build impact; overlapping edits report a semantic conflict
a structural write returns a complete receipt before commit
N concurrent --expect-root writers serialize correctly (applied/already/conflict)
```

## Phase 4 — Conditional and Field-Granular Drop Glue

Goal: solve drop placement for partial and conditional moves, lifting the v2
fail-closed rejection. Soundness prerequisite for recursion, early exit, and
loops ([SPEC_V3.md](SPEC_V3.md) §7).

Status: planned. Resolves the v2 deferred drop-glue gap.

Deliverables:

```text
dataflow that tracks per-place move state across if/case branches and loop edges
drop placement for values moved in only some branches (conditional drop glue)
drop placement for partial moves out of record fields / array elements
verify still proves drops occur exactly once under the new dataflow
```

Files likely touched:

```text
src/lowering.rs, src/verify.rs, src/layout.rs, src/backend/native.rs
src/expr.rs (evaluator drop parity)
tests/drop_glue_conditional.rs
```

Acceptance fixture and oracle:

```text
a native program moves an owned value in only some branches and partially out of
  a record; it compiles, runs, and an allocation interposer confirms exactly-once
  drop with no leak or double-free; evaluator agrees
```

## Phase 5 — Cyclic Content-Addressing: Recursion Groups

Goal: give a mutually-recursive clique a well-defined, replayable content hash —
the keystone substrate for recursion, function values, self-reference, and
packages ([SPEC_V3.md](SPEC_V3.md) §6, Pillar 1).

Status: planned. Substrate behind R1, R13, D1, D7.

Deliverables:

```text
a recursion-group / fixpoint-reference object: by-name edges within the group,
  content edges into it; the clique's content hash canonicalizes internal
  references to stable in-group identities
stable identity for the clique and for each member
birth identities are deterministic — derived from the creating migration and its
  in-migration ordinal — so identities and root hashes reproduce on rebuild
  (resolves the SPEC §29 open question on birth seeds)
verify handles recursive call graphs (effects, borrows, moves, drop ordering)
replay/export/import round-trips a recursion group with a stable hash
```

Files likely touched:

```text
src/model.rs, src/store.rs, src/types.rs, src/verify.rs, src/provenance.rs
tests/recursion_group.rs
```

Acceptance checks:

```text
a recursion-group object hashes deterministically and survives replay/export/import
verify accepts a well-formed group and rejects an inconsistent in-group reference
```

## Phase 6 — Recursion and Mutual Recursion (R1)

Goal: allow a function body to call itself and forward-declared peers, lowering
through the Phase 5 recursion-group object.

Status: planned. Resolves R1. Depends on Phase 4 (drop across recursive calls)
and Phase 5 (recursion groups).

Deliverables:

```text
name resolution admits a function's own symbol and in-group peers
lowering emits recursive/mutual calls; backend already has call/return
borrow/move/drop and effect checking across recursive call graphs
```

Files likely touched:

```text
src/types.rs, src/lowering.rs, src/verify.rs, src/expr.rs, src/backend/native.rs
examples/v3/, tests/recursion_native.rs
```

Acceptance fixture and oracle:

```text
sum/length over a recursive box<Node>, and a tree-walking expression evaluator,
  compile to native artifacts and match the reference evaluator
```

## Phase 7 — Pattern Matching Richness (R14)

Goal: extend `case` with literal, wildcard, guard, and nested patterns plus
exhaustiveness, as required by IR/AST dispatch.

Status: planned. Resolves R14.

Deliverables:

```text
literal/range patterns, `_` wildcard, `if` guards, nested destructuring
exhaustiveness checking with a deterministic diagnostic
lowering of decision trees; evaluator and backend parity
```

Files likely touched:

```text
src/expr.rs, src/types.rs, src/lowering.rs, src/verify.rs, src/backend/native.rs
tests/pattern_match_native.rs
```

Acceptance fixture and oracle:

```text
a native program dispatches on integer literals with a `_` default and a nested
  pattern; oracle agrees; exhaustiveness rejects a missing case
```

## Phase 8 — Reference Evaluator in CodeDB (Ladder Rung 0)

Goal: re-express the reference evaluator as CodeDB objects — the first
self-hosting completion and the Pillar-1 warm-up.

Status: planned. Self-hosts rung 0. Depends on Phases 6 (recursion) and 7
(patterns).

Deliverables:

```text
the lowered-IR walker, the Value model, and op evaluation, written in .cdb
a native build of the CodeDB-hosted evaluator
the determinism oracle wired to compare it against the Rust evaluator
```

Files likely touched:

```text
compiler/eval/*.cdb, std/*
tests/selfhost_eval.rs
```

Acceptance fixture and oracle:

```text
the CodeDB-hosted evaluator agrees with the Rust evaluator on the entire existing
  test corpus, yielding a three-way oracle (CodeDB-eval == Rust-eval == native)
```

## Phase 9 — Sized Integers, Bitwise, Casts, and Modulo (R5, R4, R6, R2)

Goal: the arithmetic/codec stack the importer's hashing and later codegen need.

Status: planned. Resolves R5, R4, R6, R2.

Deliverables:

```text
sized/unsigned integer types (i8/i16/i32/i64, u8/u16/u32/u64) over the layout model
wrapping vs trapping semantics defined; bitwise & | ^ ~ << >> with shift rules
numeric casts (widen/narrow/sign) with trap-or-wrap on narrowing
remainder/modulo with negative-operand and modulo-by-zero semantics
```

Files likely touched:

```text
src/types.rs, src/layout.rs, src/expr.rs, src/lowering.rs
src/backend/native.rs, src/verify.rs, src/abi.rs
examples/v3/sha256.cdb, tests/codec_native.rs
```

Acceptance fixture and oracle:

```text
sha256.cdb hashes a byte slice and matches a reference digest; fnv1a.cdb drops
  its xor/wrap emulation and still yields 0x1e225c96; ABI for new widths verifies
```

## Phase 10 — Early Exit and Error Control Flow (R7)

Goal: stop a function or loop early, for malformed-input and first-match paths.

Status: planned. Resolves R7. Depends on Phase 4 (drop across the early-exit edge).

Deliverables:

```text
a Result type with `?` propagation, and/or scoped break/continue for loops
defined interaction with drop ordering and effects on the early-exit edge
```

Files likely touched:

```text
src/expr.rs, src/types.rs, src/lowering.rs, src/verify.rs, src/backend/native.rs
examples/v3/tokenizer.cdb, tests/early_exit_native.rs
```

Acceptance fixture and oracle:

```text
tokenizer.cdb rejects malformed input and exits its loop early; verify proves
  drop/borrow correctness across the early-exit edge; oracle agrees
```

## Phase 11 — Unbounded Loops (R8)

Goal: a condition-driven loop for worklist/fixpoint passes (and, later, servers).

Status: planned. Resolves R8. Depends on Phases 4 (loop-carried drops) and 10 (break).

Deliverables:

```text
`while cond do body` and/or `loop { ... break }`, lowering to real backend loops
loop-carried borrow/drop/effect checking; verify handles non-terminating control
```

Files likely touched:

```text
src/expr.rs, src/types.rs, src/lowering.rs, src/verify.rs, src/backend/native.rs
tests/while_native.rs
```

Acceptance fixture and oracle:

```text
a native fixpoint/worklist pass iterates until a condition and matches the oracle
```

## Phase 12 — Strings and Integer Formatting (R15, R3)

Goal: a real string surface and int<->string formatting, as stdlib over the
v2 dynamic buffer — required for text processing and diagnostics.

Status: planned. Resolves R15, R3.

Deliverables:

```text
std.string: index, compare, concat, substring, push, bytes<->string
std.fmt: i64<->string (decimal, plus hex/unsigned), and a write-to-buffer variant
```

Files likely touched:

```text
std/string.cdb, std/fmt.cdb (new), std/core.cdb
tests/string_native.rs, tests/fmt_native.rs
```

Acceptance fixture and oracle:

```text
a native program concatenates, compares, and indexes strings; format/parse
  round-trips i64 over a range including negatives, with no hand-rolled digit table
```

## Phase 13 — Array Fill / Repeat Initializer (R9)

Goal: `[value; N]` so large fixed buffers are expressible as values.

Status: planned. Resolves R9.

Deliverables:

```text
`[expr; N]` parsing, type rules, and lowering to a fill/memset over the array place
```

Files likely touched:

```text
src/expr.rs, src/types.rs, src/lowering.rs, src/backend/native.rs
tests/array_fill_native.rs
```

Acceptance fixture and oracle:

```text
`[0; 1024]` type-checks and lowers; http_server.cdb uses a stack array buffer
  instead of malloc
```

## Phase 14 — Generics / Parametric Types (R11)

Goal: type parameters on fn/record/enum with monomorphization at lowering — the
one large rock the compiler genuinely needs (`Vec<T>`, `Option<T>`, `Result`).

Status: planned. Resolves R11. Designed as its own pass.

Deliverables:

```text
type parameters on functions, records, and enums (constraint-free to start)
monomorphization at lowering; stable derived identity for each instance
the interface_hash/implementation_hash split applied to instances
verify recomputes and validates instances; provenance traces instance -> generic
```

Files likely touched:

```text
src/types.rs, src/model.rs, src/lowering.rs, src/verify.rs
src/provenance.rs, src/migrations.rs, src/backend/native.rs
tests/generics_native.rs
```

Acceptance fixture and oracle:

```text
one generic Option<T> (or Vec<T>) compiles natively at two or more instantiations;
  blame/why traces an instance back to its generic definition
```

## Phase 15 — Self-Hosted Front-End to Lowered IR (Ladder Rung A)

Goal: express the front half of the compiler as CodeDB objects and meet the Rust
native backend at the lowered-IR seam — the mixed compiler.

Status: planned. Self-hosts rung A. Depends on Phases 6, 7, 9–14.

Sub-stages (each independently oracle-checked at its artifact):

```text
15a importer: source/apply -> semantic objects   oracle: object-hash equality
15b type check -> typed expressions               oracle: typed-object equality
15c borrow/effect/move/drop check                 oracle: same accept/reject + diag
15d layout                                         oracle: layout-JSON equality
15e lowering -> lowered IR                         oracle: IR-hash equality
```

Files likely touched:

```text
compiler/front/*.cdb, std/*
tests/selfhost_frontend.rs
```

Acceptance fixture and oracle:

```text
the CodeDB front-end lowers the acceptance corpus to IR that is hash-identical to
  the Rust front-end's; the Rust native backend then builds it to identical
  binaries (mixed compiler)
```

## Phase 16 — Process Arguments / argv (R12)

Goal: a richer entry signature so the self-hosted compiler reads a source path.

Status: planned. Resolves R12.

Deliverables:

```text
target argv/envp threaded into an entry signature (e.g. main(args: slice<...>))
capability surfacing of args in build/entry metadata
```

Files likely touched:

```text
src/lowering.rs, src/backend/native.rs, std/io.cdb, std/platform/*.cdb
src/build_plan.rs, tests/argv_native.rs
```

Acceptance fixture and oracle:

```text
a native program echoes its first command-line argument; the self-hosted
  front-end accepts a source filename argument
```

## Phase 17 — Self-Hosted Native Object Emission (Ladder Rung B)

Goal: express native object emission (lowered IR -> `.o`) as CodeDB objects. The
large back-half rung; staged last but not optional.

Status: planned. Self-hosts rung B. Depends on Phase 9 (bitwise/sized/casts) and 15.

Deliverables:

```text
machine-code encoders for x86_64 and arm64 written in .cdb
relocation and section emission; ELF and Mach-O object writers in .cdb
the determinism oracle wired to compare emitted bytes
```

Files likely touched:

```text
compiler/backend/*.cdb, std/*
tests/selfhost_emit.rs
```

Acceptance fixture and oracle:

```text
the CodeDB-hosted emitter produces `.o` bytes identical to the Rust emitter for
  the acceptance corpus on both targets
```

## Phase 18 — Self-Hosted Link Plan (Ladder Rung C)

Goal: express the link plan as CodeDB objects, closing the ladder — CodeDB
compiles itself end to end, checked against Rust at every seam.

Status: planned. Self-hosts rung C. Depends on Phase 17.

Deliverables:

```text
link-plan construction (reachable externs, capabilities, ABI symbols) in .cdb
the determinism oracle wired to compare link-plan JSON
a self-host bootstrap fixture: CodeDB compiles the corpus with no Rust stage
```

Files likely touched:

```text
compiler/link/*.cdb, std/*
tests/selfhost_link.rs, tests/selfhost_bootstrap.rs
```

Acceptance fixture and oracle:

```text
the CodeDB-hosted link plan equals the Rust linker driver's; the fully self-hosted
  pipeline reproduces the Rust compiler's binaries for the acceptance corpus
```

## Cross-cutting policy

The v2 native backend policy carries forward.

Allowed conservative choices:

```text
pass large aggregates indirectly; hidden return slots
stack slots instead of register allocation where simpler
simple bounds checks and drop glue; link the small platform capsule
```

Not allowed:

```text
fall back to interpreter execution
hide language semantics in opaque host calls
compile through a projection (emit-c or source text)
claim stage self-hosting without the determinism oracle
claim feature completion without native-required tests
skip verification for memory/layout/identity features
delete or bypass the Rust compiler (it stays as stage-0 and oracle)
treat the committed .cdb projection as authoritative source
commit the SQLite database as source (it is a disposable cache)
```

V3 additions, applied every phase:

```text
each phase adds a verify check that catches at least one malformed new object
each new language feature is native-complete before it is counted done
each new construct defines its stable identity and how provenance attaches
each ladder rung is gated by the determinism oracle
the committed .cdb is a checked view; CI gates import -> verify -> re-export and a root_hash pin
```

## Suggested milestone cuts

### Milestone V3.0 — Foundations

Includes: Phases 1–3 (docs, architecture paydown, agent spine). Status: complete.

```text
Success: features add with a small edit surface, and concurrent agents build them
  through structural edits with proof-carrying receipts and semantic merge.
```

### Milestone V3.1 — Sound Recursive Frontier

Includes: Phases 4–7 (drop glue, recursion groups, recursion, pattern matching).

```text
Success: recursion compiles native; drops occur exactly once across conditional
  and recursive paths; verify handles recursive call graphs.
```

### Milestone V3.2 — Self-Hosted Oracle

Includes: Phase 8 (reference evaluator in CodeDB, rung 0).

```text
Success: CodeDB-eval == Rust-eval on the corpus — a three-way oracle.
```

### Milestone V3.3 — Expressiveness for a Front-End

Includes: Phases 9–14 (ints/bitwise/casts/modulo, early exit, loops, strings/fmt,
array fill, generics).

```text
Success: sha256.cdb and tokenizer.cdb compile native; the language can express a
  compiler front-end.
```

### Milestone V3.4 — Self-Hosted Front-End

Includes: Phases 15–16 (rung A mixed compiler, argv).

```text
Success: the CodeDB front-end lowers to IR hash-identical to Rust's; mixed
  compiler builds identical binaries.
```

### Milestone V3.5 — Full Self-Hosting

Includes: Phases 17–18 (rung B emission, rung C link).

```text
Success: byte-identical .o and link plan; the self-hosted pipeline reproduces the
  Rust compiler's binaries — CodeDB compiles itself.
```

## Risks and mitigations

### Risk: cyclic content-addressing design is wrong; clique hashing is unstable

Mitigation:

```text
specify the recursion-group object before building on it
prototype its hashing and replay/export/import round-trip first (Phase 5)
make in-group identity canonical and verify-recomputable
```

### Risk: conditional/field-granular drop glue blocks recursion, loops, early exit

Mitigation:

```text
solve it as a standalone phase (4) with interposer leak/double-free tests
keep the v2 fail-closed rejection until the dataflow is proven
sequence every dependent feature (R1/R7/R8) strictly after Phase 4
```

### Risk: generics x identity explode the type system and provenance

Mitigation:

```text
design instance identity and the interface/implementation split first
monomorphize at lowering; verify recomputes instances
stage generics on its own (Phase 14), not interleaved with other features
```

### Risk: native-emission self-hosting (rung B) is huge with little thesis novelty

Mitigation:

```text
stage it last; keep the mixed compiler (rung A) as a usable plateau
rely on the byte-equality oracle to keep it honest
do not scope it away by lowering to C
```

### Risk: the extend-everything tax slows velocity and causes agent merge conflicts

Mitigation:

```text
do the Phase 2 paydown (single source of truth + oracle conformance) up front
keep the evaluator and backend pinned together by the conformance harness
```

### Risk: agent merge fidelity is insufficient; agents fall back to editing text

Mitigation:

```text
gate multi-agent work behind the Phase 3 minimum semantic merge
forbid treating projection text as source; require structural operations
keep receipts complete so an agent can bind to the root it inspected
```

### Risk: self-hosting tempts scope creep into the v4 agent product

Mitigation:

```text
hold the SPEC_V4 fence; keep the agent spine at "build one compiler" sizing
park "a bit more of the agent product" in SPEC_V4, not v3
```

## Out of scope for initial v3

Initial v3 should not require:

```text
async/concurrency (D3)
high-performance optimizer (D4)
full DWARF debug info (D6)
full struct-by-value C ABI beyond FFI needs (D5)
floating point (R10) — not required to self-host
the full agent-native platform (semantic-review-as-a-service, distribution) — v4
compiling through C, or deleting the Rust compiler
```

These can follow once the language is self-hosting and the substrate is proven on
the compiler itself.
