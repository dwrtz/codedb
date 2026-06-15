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

Status: implemented. Conditional drop glue is realized as compensating drops at
`if`/`case` merges and field-granular residual drops at scope exit — all static,
no runtime drop flags (answering the SPEC §8 open question: drops stay
compiler-generated artifacts in lowering, not semantic objects). The semantic
checker (`src/types.rs`) now admits asymmetric conditional moves and partial
record-field moves (adding `ExprUse::ProjectionBase` so reading a sibling field
of a partially-moved aggregate is allowed); lowering (`src/lowering.rs`) tracks
per-place move state (`MovedPlace`) and the lowered-IR verifier proves
exactly-once with per-branch isolation + merge. `tests/drop_glue_conditional.rs`
runs a `box`-heap program natively (no double-free) confirming exactly-once under
conditional moves, field-granular moves, and the two COMBINED (a record field
moved in only some `if`/`case` branches while a sibling field stays live — the
branch-merge compensation drops the live remainder without recursing into the
untouched non-record sibling); the five former-rejection tests in
`tests/move_drop.rs` are now acceptance tests. Array-element partial moves stay
fail-closed (records only).

Deliverables:

```text
dataflow that tracks per-place move state across if/case branches and loop edges
drop placement for values moved in only some branches (conditional drop glue)
drop placement for partial moves out of record fields / array elements
the lowered-IR verifier proves drops occur at most once (no double-free / no
  use-after-move) under the new dataflow; the no-leak half of exactly-once rests on
  lowering's static drop placement and is now confirmed at runtime by an allocation
  interposer (see the acceptance note)
```

Files likely touched:

```text
src/lowering.rs, src/verify.rs, src/layout.rs, src/backend/native.rs
src/expr.rs (evaluator drop parity)
tests/drop_glue_conditional.rs, tests/leak_interposer.rs
```

Acceptance fixture and oracle:

```text
a native program moves an owned value in only some branches and partially out of
  a record; it compiles, runs (a double-free would abort the run), and the evaluator
  agrees. Exactly-once drop placement is checked statically in lowering, pinned by
  per-fixture lowered-IR drop assertions, and confirmed at runtime by an allocation
  interposer (`tests/leak_interposer.rs`) that counts malloc/free in the built binary:
  for a tunable box program the net (alloc - free) is invariant to the allocation
  count, so a skipped-drop leak — which the double-free guard cannot see — is caught
```

## Phase 5 — Cyclic Content-Addressing: Recursion Groups

Goal: give a mutually-recursive clique a well-defined, replayable content hash —
the keystone substrate for recursion, function values, self-reference, and
packages ([SPEC_V3.md](SPEC_V3.md) §6, Pillar 1).

Status: implemented (functions). A `RecursionGroup` object kind plus an
`Operation::CreateRecursionGroup` bind a whole clique's symbols, signatures, and
names before any body is type-checked, so members may call each other. Member
birth identities derive deterministically from the creating migration's parent
history and the member's in-group ordinal (`recursion_group:{ordinal}`), so the
clique reproduces on rebuild. Ordinals are assigned by canonical clique structure
(colour refinement over the call graph with peer identities erased), not source
declaration order, so the group's content hash is order-independent and
import→export→import is a fixpoint (two textual orderings of one clique dedup).
Recursion groups are an internal representation:
the importer detects recursive SCCs (`analyze_recursion_groups` + Tarjan) and
emits the op, while members project back as ordinary `fn`s and non-recursive
functions keep their original one-op-per-fn lowering (no migration-history
churn). Verify validates a group (members resolve; each member's definition is a
`FunctionDef` of its symbol — rejecting duplicate members and inconsistent
in-group references) and bundle export/import follows group refs.
Mutually-recursive *type* definitions (D1) remain functions-only follow-on. See
`tests/recursion_group.rs`.

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

Status: implemented. Self- and mutual recursion compile to native artifacts and
match the reference evaluator: name resolution admits a function's own symbol and
in-group peers (via the Phase 5 atomic clique creation); call lowering and the
backend were already by-symbol; the per-function verify walks are intra-procedural
and the single inter-procedural effect check is satisfied inductively over the
clique, so the cyclic call graph needs no fixpoint. `tests/recursion_native.rs`
covers factorial, fibonacci (two recursive calls), mutual is_even/is_odd, and a
recursive `box<Node>` builder (recursion + recursive type layout + recursive drop
glue). A latent overflow — `collect_reference_regions_in_type` lacked a cycle
guard and recursed forever on a recursive return type — was fixed with a `seen`
set. Traversing a recursive `box` heap *by case* now works too: an `unbox`
(deref-by-move) builtin moves a `box<T>` payload back to an owned `T` (copying the
payload out and freeing the shell, like `Box::into_inner`), and a `case` arm may
bind and move a move-only `box` payload out of a consumed (param/local) scrutinee —
so `length` over a recursive `box<Node>` compiles native and matches the evaluator,
with `leaks` reporting `0 leaks` and no double-free. Per-node-data structures (sum
over a cons-list, a tree-walking evaluator) additionally need mutually-recursive
*type* definitions (D1, e.g. `Cons`↔`List`), which remain a follow-on. Depends on
Phase 4 and Phase 5.

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

Acceptance fixture and oracle (met for the recursive-`box` heap; see Note):

```text
length over a recursive box<Node> compiles to a native artifact, traverses the heap
  by case + unbox, and matches the reference evaluator (0 leaks, no double-free)
```

Note: the deref-by-move blocker is resolved. `unbox(b: box<T>) -> T` (a builtin,
lowered to a new `UnboxMove` op that copies the payload to an owned slot then frees
the box shell — heap read strictly before free, on both x86_64 and arm64) and
move-only `box` case-arm binding together make case-traversal of a recursive
`box<Node>` heap expressible and native (`tests/recursion_native.rs`).

Mutually-recursive *type* definitions (D1) are now supported too, so per-node-data
structures work: a `Cons`↔`List` cons-list `sum` and a tree-walking expression
evaluator (`Expr`↔`Pair`) compile native and match the evaluator
(`tests/recursive_types.rs`). A `CreateTypeGroup` migration creates a type clique
atomically — every member's name is bound (with a placeholder definition) before any
definition is resolved, mirroring `CreateRecursionGroup` for functions; `box` breaks
the size cycle and members reference each other by symbol (no hash cycle). Member
ordinals are canonical (individualization-refinement, shared with the function path),
so the clique hash is source-order-independent and import→export→import is a fixpoint.
A supporting fix: an enum-variant payload now coerces a structural record/enum/array
literal to the variant's nominal type (so `List::cons(box_new({ ... }))` works), which
benefits all enums. Inline (non-`box`) move-only enum payloads still stay fail-closed.
These unblock more of the Phase 8 self-hosted evaluator.

## Phase 7 — Pattern Matching Richness (R14)

Goal: extend `case` with literal, wildcard, guard, and nested patterns plus
exhaustiveness, as required by IR/AST dispatch.

Status: implemented (scalar literal + wildcard + exhaustiveness). `case` now
dispatches on an `i64`/`bool` scrutinee by literal patterns plus a `_` wildcard,
preserved as a rich typed node (so the `.cdb` projection round-trips and the
`FunctionSourceMatches` postcondition holds) and desugared to an `if`/`eq` chain
at lowering — reusing the existing backend with no new code generation, and
inheriting Phase 4 conditional drop glue for arm bodies. Exhaustiveness is
checked with a deterministic diagnostic (an `i64` case needs a `_`; a `bool` case
must cover true/false or have a `_`; enum coverage as before). `case`-in-arm
nesting works and round-trips — a nested `case` in a non-last arm is parenthesized
so the projection re-parses. Evaluator, native, and `trace`/`debug` parity in
`tests/pattern_match_native.rs` and `tests/trace.rs`. Range patterns (`lo..hi`
exclusive / `lo..=hi` inclusive on an `i64` scrutinee, negative bounds allowed) are
now implemented — desugared to a `scrutinee >= lo && scrutinee {<,<=} hi` test in
the same `if`/`eq` chain, first-match order, projection round-trips, exhaustiveness
still requires `_` (a finite set of ranges cannot cover `i64`). `if` guards and
nested enum-destructuring patterns remain documented follow-on R14 surface.

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

Status: COMPLETE — the first self-hosting completion (milestone V3.2). The
CodeDB-hosted evaluator (compiler/eval/eval.cdb, ~1100 lines of .cdb built
through the staged plan below) compiles natively and is result-equal to the
Rust evaluator on the operator-conformance sweep (one fixture per registry
kind), the per-feature scalar/aggregate/heap fixtures, and the qualifying
example corpus — including the COMPLETE sha256 digest, all eight words —
with the native backend as the transitive third leg; the committed sources
pass the §11 checked-view gate (tests/selfhost_eval.rs). Self-hosts rung 0.
Depends on Phases 6 (recursion) and 7 (patterns). Two design pins, settled:
the CodeDB-hosted evaluator walks the **lowered IR** (SPEC_V3 §5's "smallest
recursive IR-walker"), not typed expressions — the IR is layout-resolved
(explicit offsets/slot sizes, desugared patterns, monomorphized generics,
explicit drops/traps), so the Value model is a byte machine over simulated
memory and no type/layout/pattern machinery needs re-expression at this rung;
and it consumes a new deterministic flat-binary **CIR** artifact rather than
parsing the canonical-JSON IR. Landed substrate: (1) `string_set(s, i, b)` —
the random-access write twin of `string_get`, full native-completion stack,
making a `string` usable as mutable byte memory (simulated frames/heap);
`tests/string_set_native.rs`. (2) `src/cir.rs` + `emit-cir <db> <entry>
--target --out`: the lowered-IR closure of an entry as flat bytes (interned
string/data pools; function table sorted by symbol hash with the entry named
by index; per-function type/value tables; one stable opcode byte per lowered
op, blocks nested inline) with calls pre-resolved to function indices, type
hashes to type-table indices, and value ids to dense indices. Every emission
decodes its own output and fails unless the decoded IR is structurally
identical to the lowered original (the built-in honesty gate); bytes are
deterministic across re-emission AND an independent rebuild from the same
source; extern-reachable closures are rejected fail-closed (the rung-0 corpus
is the Rust evaluator's domain, which cannot execute externs either);
monomorphic instances appear as ordinary functions, templates never do.
`tests/cir_artifact.rs`; format doc in `src/cir.rs` + `compiler/eval/README.md`.

Landed since: (3) CIR **consumer columns** — decoder-verified derived metadata
(per-type `meta_kind`/`meta_size` classified from the well-known scalar hashes
and layout rows; per-binary/unary `verb`/`width`/`signed` from the operator
registry) plus dense-slot validation, so the `.cdb` walker never interprets
hash strings or kind names. (4) **Stage 1 of the `.cdb` evaluator itself**
(`compiler/eval/eval.cdb`): the execution design is pinned in
`compiler/eval/README.md` (single-string memory map, backend-mirroring 8-byte
value cells, frame model, bump heap with no-op drops, `ok:`/`trap:` output
protocol, move-threading discipline), and the loader walks a real CIR — stdin
through a 1-byte bounce buffer (a path argument loses to the v0 frame budget:
every value id costs 8 frame bytes, so `[0x0; N]` buffers cost ~24N), then
magic/version, both pools, the function table, and the entry's fixed-width
section tables — all NATIVE, gated by a five-number probe (function count,
entry index, entry op/param/local counts) that must match `emit-cir`'s summary
on the tokenizer and sha256 examples (`tests/selfhost_eval.rs`). The walk also
exercised real `.cdb` authoring: the structural-literal-in-let anchoring
gotcha and the value-id frame tax are both documented in the source.
(5) **Stage 2 — the scalar core EXECUTES.** The `.cdb` evaluator builds
per-function metadata (frame sizes, local offsets, return-type meta) at load,
then runs the entry over `[param cells | locals | value cells]` frames on a
simulated stack: consts, params, every binary/unary operator width and
signedness — semantics inherited BY CONSTRUCTION by casting the canonical
cells to the .cdb type of the op's consumer columns and applying the .cdb
operator itself — `int_cast` renormalization, `if` with block skipping (a
structure-only `skip_op` walker doubles as the not-taken-branch validator),
scalar load/store/copy/move, borrows/derefs as address cells, bounds checks,
calls by function-table index (callee frames bump past the caller's cells),
return/early-return unwinding, and div/mod trap parity. Gate:
`tests/selfhost_eval.rs` asserts CodeDB-eval == Rust-eval on scalar
control-flow/recursion programs (early return, fib, width edges, u64
rendering) AND on a generated conformance sweep with one fixture per
`codedb::operator_kinds()` entry (the coverage assert makes a kind without a
fixture fail), with div0/mod0 trapping identically on both sides. The
authoring tax surfaced one more backend reality, now documented: every op's
value id costs 8 frame bytes, so big dispatchers must split (`exec_op_*`
routers + one-literal-site record constructors `mk_rv`/`mk_ex`).
(6) **Stage 3 — aggregates execute.** Records/enums/arrays/slices/static
data/case/fold/loop, mirroring the backend's value model exactly: an
aggregate value IS its address (load/move/copy alias the cell; only `store`
copies bytes), addr-of ops use the places' explicit offsets (index stride =
element size rounded to its layout alignment), enum tags are 8 bytes at
offset 0, `case` treats its last arm as the default, the `fold`/`loop`
drivers iterate over accumulator/index/item locals with early returns
propagating out of iteration bodies, and calls implement the hidden-return-
slot + indirect-param-copy ABI (callee frames carry ownership copies of
aggregate-indirect params; the param cell keeps the caller's address —
addr_of_param(indirect) yields the copy). A new CIR consumer kind splits
slices from other aggregates (the fold target deref). Gate: tokenizer
(ok:123 / early-exit -1 / empty) and sha256's digest word, plus a
per-feature aggregate fixture (nested records, enum payloads with a default
arm, runtime-indexed arrays, fill + array_set, fold with an early return
from its body, static slices, aggregate params/returns) — all result-equal
to the Rust evaluator; param-taking and aggregate-result entries stay
outside the protocol fail-closed. Two more v0 frame realities documented:
calls cap at 8 machine parameters (driver headers ride in Copy records) and
the dispatcher/driver split keeps every frame under the 4095-byte budget.
(7) **Stage 4 — the heap executes.** `heap_alloc` bumps (the pointer cell
sits at the stack/heap boundary; drops and box-shell frees are validated
no-ops — result equality cannot observe them and the bump heap makes
use-after-free unrepresentable), `unbox` copies payloads out (and BY-VALUE
records — small aggregates the ABI passes `by_value` — ride in cells as raw
byte patterns, the backend's `passes_indirect == false` path), and vec/
string buffers run over `{ptr, len, capacity}` headers located by a second
round of consumer columns (per-layout buffer offsets + element size/stride,
derived from the layout metadata so the walker never parses JSON). Buffer
ops trap at capacity exactly like the native runtime — a DOCUMENTED
divergence from the growable Rust-eval string model, pinned by its own test
(a correctly-sized program never reaches the edge, per Phase 12). argv
forwards 1:1 (the CIR rides stdin). Gate: boxes (aggregate, by-value, and a
recursive cons list through enum payloads), vec/string ops, std.fmt's
negative-domain round-trip over the bump heap, and argv parity against
--process-arg — all result-equal to the Rust evaluator. With Stage 4 every
one of the 56 CIR opcodes is routed; what remains for V3.2 is Stage 5: the
corpus-wide manifest sweep and the §11 checked-view gate for
compiler/eval/*.cdb. (8) **Stage 5 — the corpus sweep + checked-view gate.**
The manifest covers every committed example whose entries are extern-free,
parameterless, and scalar-result (booleans, discount, fnv1a, the tokenizer,
and the complete sha256 digest), each three-way result-equal; and the §11
gate holds — the evaluator's two-import bootstrap (std/fmt.cdb +
compiler/eval/eval.cdb) consolidates in one import→export→import pass to a
byte-stable canonical projection and a fixpoint root. Phase 8 is COMPLETE.
Known cost: the selfhost_eval suite is the heaviest native suite (~20 min;
dominated by the evaluator import+verify per test process and the per-entry
CLI rounds) — caching the built evaluator across runs keyed on the source
hash is the obvious follow-on if it grows.

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

Status: implemented. Resolves R5, R4, R6, R2 — all-width operators with
canonical sign/zero-extended slot form on both arches, `to_*` cast builtins,
hex literals (signed widths read them as bit patterns), MIN literals via the
negated-literal fold, and the per-width conformance fixtures.
`fnv1a.cdb` native == eval; `sha256.cdb` (rolled into loops) native == eval ==
the reference digest (tests/codec_native.rs).

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

Status: implemented (early `return`). Resolves R7. Settles the SPEC_V3 §14 open
question — `return` lands first, as the early-exit primitive both `?`-propagation
and scoped break/continue desugar toward (break/continue follow with loops in Phase
11; `Result` + `?` await generics, Phase 14). In this expression-oriented language
(no statements/loops yet) the load-bearing case is a `return` in a NON-tail position
— an `if` branch or `case` arm whose value flows into a `let` continuation — which
abandons that continuation. That is the genuine early exit a plain `if` cannot
express without hoisting the continuation, and it exercises the real early-exit edge
SPEC_V3 §7 requires verified.

`return <value>` is a new `RawExpr`/typed node (`{expr_kind: "return", value,
type}`) typed as its operand's type, so it slots into the existing `if`/`case`
join, which is now divergence-aware: a branch that always `return`s ("diverges")
fixes no result type and the non-divergent branch does (computed structurally by
`raw_expr_diverges` / `typed_expr_diverges`, never a `never`/bottom type). A
`return` is well-formed only in a "block-result" position (function body, `if`/`case`
branch, `let` body); a `return` in a value/operand/condition/scrutinee/`let`-value/
`fold`-body position fails closed before typing (`validate_return_positions`). The
borrow/move checker excludes a divergent branch from the merge (its moves never
reach the continuation), and the escape check treats the `return` operand as an
escaping return value.

Lowering adds a `LoweredOp::EarlyReturn` (a mid-stream control-flow terminator,
distinct from the single terminal `Return`): at the `return`, every owned value
still live is dropped — in-scope locals innermost-first, then params, each
respecting what the operand consumed — exactly the drops the `let`-scope-exit and
function-end param scaffolds place on the fall-through path, emitted here on the
early-exit edge instead (SPEC_V3 §7). `lower_if`/`lower_case` (and the desugared
scalar-`case` chains) merge only non-divergent branches; a fully-divergent body
skips the (now unreachable) param scaffolds + terminal return. Divergence is
detected structurally from the ops (`lowered_ops_diverge`), so no IR field is added
and existing lowered-IR hashes are byte-stable. The backend emits `EarlyReturn` as
the terminal return's value placement + the self-contained epilogue inline (no jump
or label — multiple `ret`s per function are fine; the post-branch merge tail is
unreachable dead code). The lowered-IR verifier proves at-most-once across the edge
(divergence-aware `if`/`case`, `EarlyReturn` validated as a return of the function's
return type). The evaluator and tracer unwind an early `return` via a thread-local +
sentinel error caught at the function-call boundary (`eval_symbol`/`trace_symbol`),
so the reference evaluator stays a faithful oracle with no per-call-site signature
churn. Round-trips, native (x86_64 + arm64), `verify`, `trace`, and replay/export/
import all support it. `examples/v3/tokenizer.cdb`, `tests/early_exit_native.rs`, and
a `tests/leak_interposer.rs` early-exit case are the acceptance gates.

Deliverables (delivered):

```text
early `return <value>`: the early-exit primitive (Result + `?` is its sugar, deferred
  to generics; break/continue is its loop form, deferred to Phase 11)
defined interaction with drop ordering and effects on the early-exit edge
```

Files likely touched:

```text
src/expr.rs, src/types.rs, src/lowering.rs, src/verify.rs, src/backend/native.rs
examples/v3/tokenizer.cdb, tests/early_exit_native.rs
```

Acceptance fixture and oracle (met):

```text
tokenizer.cdb rejects malformed input and exits its loop early; verify proves
  drop/borrow correctness across the early-exit edge; oracle agrees
```

`examples/v3/tokenizer.cdb` is a recursive decimal tokenizer that early-returns -1
on a non-digit byte, abandoning the rest of the scan (the recursion stands in for
the loop, which is Phase 11): `tokenize "123" = 123`, `tokenize "1x3" = -1`. It
compiles native and matches the evaluator (`tests/early_exit_native.rs`).
`tests/early_exit_native.rs` additionally pins early `return` in `if` branches and
`case` arms, the non-tail continuation skip, the box-drop-across-the-edge case
(double-free-on-run verified, the drop pinned in the lowered IR), the fail-closed
position rejection, projection round-trip, and trace parity;
`tests/leak_interposer.rs::early_return_drops_live_box_on_the_exit_edge_at_runtime`
confirms the no-leak half at runtime scale.

## Phase 11 — Unbounded Loops (R8)

Goal: a condition-driven loop for worklist/fixpoint passes (and, later, servers).

Status: implemented. Resolves R8. In this expression language a loop must carry
state (there is no mutation), so the surface is `loop acc = init while cond do body`
— the condition-driven counterpart of `fold`: the accumulator `acc` starts at
`init`; while `cond(acc)` (a `bool`) holds, `acc` becomes `body(acc)` (the next
accumulator, same type); the loop yields the final `acc`. Both `cond` and `body` see
`acc`. A record accumulator carries several loop-varying values (e.g. `{ state, i }`
for a worklist over an array read by the loop index), context-typed/anchored to the
named accumulator type exactly like a `let acc: T = <init>` (so a `{ acc: 0x0, .. }`
sized-int field takes its declared width).

The MVP shipped with `fold`'s soundness envelope (copyable accumulator, no body
moves); LOOP-CARRIED DROP GLUE (the former #4 item) has since landed. The
accumulator may now be MOVE-ONLY: each iteration the body either consumes it
wholly (its drop obligation transfers — branch-conditional consumption is made
uniform by the if/case merge compensation) or lowering appends a drop of the
old value to the body block, so the back-edge store never overwrites a live
owned value — exactly-once either way, pinned by tests/leak_interposer.rs.
Per-iteration body locals (in `loop` AND `fold` bodies) may also move; their
scoped drop glue re-executes each iteration. Still rejected, permanently:
moves of storage that OUTLIVES one iteration (params/outer locals — the move
would repeat), partial accumulator projections, and any accumulator move from
`cond` (it runs once more than the body, so its final evaluation would consume
the loop's own result). A conditional `return` inside the body exits the whole
function, dropping a still-live accumulator on the early-exit edge.
`break`/`continue` stay deferred (the condition is the loop's only structured
exit). A new `LoweredOp::Loop` lowers to a real backend loop on x86_64 and
arm64 (seed the accumulator slot, re-run the `cond` block each iteration and
exit when it is false, else run the `body` block and store its result back) —
`lower_loop`/`emit_loop` mirror `lower_fold`/`emit_fold` minus the index/item
bookkeeping. The borrow checker scopes `acc` as a loop-local and gates
iteration moves at the move-recording site; the lowered-IR verifier checks the
cond is `bool`, the body matches the accumulator, and that the blocks consume
only per-iteration storage (plus the whole accumulator, body-only) — so verify
tolerates non-terminating control. The reference evaluator and tracer iterate
the loop, guarded by a generous per-loop iteration ceiling
(`MAX_EVAL_LOOP_ITERATIONS`) that converts a non-terminating loop into a clean
error — an oracle-robustness bound, not a native limit (the backend runs the
loop unbounded), mirroring the recursion ceiling. Effects propagate through
`init`/`cond`/`body`; round-trips, `verify`, `trace`, and replay/export/import
all support it.

Deliverables (delivered, including the follow-on):

```text
`loop acc = init while cond do body` lowering to real backend loops (x86_64 + arm64)
loop-carried borrow/effect checking; verify tolerates non-terminating control
loop-carried drop glue: move-only accumulators (consume-or-back-edge-drop) and
  per-iteration body-local moves in loop AND fold bodies, leak-interposer-pinned
(deferred: break/continue — the condition is the exit, and the early-exit
 machinery is Phase 10's `return`, which works inside loop bodies)
```

Files likely touched:

```text
src/expr.rs, src/types.rs, src/lowering.rs, src/verify.rs, src/backend/native.rs
tests/while_native.rs
```

Acceptance fixture and oracle (met):

```text
a native fixpoint/worklist pass iterates until a condition and matches the oracle
```

`tests/while_native.rs` pins the fixpoint/worklist acceptance natively (eval ==
native): scalar fixpoints (count-up, double-until), a record accumulator (Collatz
step count), a worklist iterating a fixed array by the loop index, a u32 accumulator
with wrapping arithmetic + a u32 array read (the codec-shaped round loop), the
zero-iteration edge, the fail-closed move-only-accumulator and body-move rejections,
projection round-trip, and trace parity. (`examples/v3/sha256.cdb` now rolls BOTH its
64 compression rounds AND its 48-word message schedule into loops over a Copy
`array<u32, 64>` accumulator: the compression loop folds a Copy `State` reading the
round-key and schedule arrays by index, and the schedule loop fills the array by index
with `array_set` — the array-update primitive that closed the last gap. The whole hash
compiles native, eval == native on every word — see the array-update note below and
`tests/codec_native.rs`.)

Array update (`array_set`, the SHA-256 schedule enabler). `array_set(arr, i, v)` is a
functional update of one element of a fixed Copy array: it yields a NEW `array<T, N>`
equal to `arr` with element `i` set to `v`. Like `[value; count]` (R9), the element
must be a non-reference Copy value with trivial drop — so the array is Copy (a `loop`
can carry it as its accumulator), the source copy is a blind whole-slot copy, and
overwriting element `i` is leak-free. The index is bounds-checked at runtime (a literal
out-of-range index is rejected at type-check). It is a recognized builtin call (no new
grammar): a typed `array_set` node lowers by copying the source array into the
destination slot, then a bounds-checked indexed `Store` — reusing the array-init +
`array_index` machinery, so there is NO new lowered op and NO new backend codegen. The
full native-completion stack supports it (type check + the six analyses, evaluator,
projection `array_set(..)` + reconstruction, three-child walkers, lowering, native
x86_64 + arm64, `verify`, `trace`, replay/export/import). It is the array counterpart of
`string_push`/`vec_push` for a Copy buffer and the substrate for a worklist that builds
an array by index. `tests/array_set_native.rs` pins it (chained literal-index updates, a
runtime-index loop building an array, a u32 distinct-layout array, the lowered-IR shape,
the projection fixpoint, and the fail-closed rejections); `tests/codec_native.rs` pins
the SHA-256 acceptance (eval == native on all eight words of `sha256("abc")`).

## Phase 12 — Strings and Integer Formatting (R15, R3)

Goal: a real string surface and int<->string formatting, as stdlib over the
v2 dynamic buffer — required for text processing and diagnostics.

Status: implemented. Resolves R15, R3. Three new dynamic-string primitives join
the existing `string_new`/`string_len`: `string_with_capacity(n)` allocates an
empty (len 0) buffer with a *runtime* capacity `n` (unlike the literal-capacity
`vec_new`/`string_new`); `string_push(s, b)` appends a `u8`, trapping at capacity
(no realloc, like `vec_push`); `string_get(s, i)` is a bounds-checked indexed `u8`
read. They mirror the `vec<T>` ops over the same `{ptr,len,cap}` heap buffer — the
backend push/get emitters are now shared `emit_buffer_{push,get}_{x86,arm64}` helpers
parameterized by buffer kind, and the runtime-sized malloc is a new
`emit_string_with_capacity_*` (it allocates `max(capacity, 1)` bytes so `malloc(0)`
— which may return NULL and trap — is never called). Everything else is `.cdb`
stdlib: `std/string.cdb` adds index/length/push wrappers, `push_range`, `concat`,
`substring`, byte-wise `eq`, and lexicographic 3-way `compare`; `std/fmt.cdb` adds
`i64_to_string` (signed decimal) and `string_to_i64` with **no hand-rolled digit
table** — the digit codec is `'0' + d` / `b - '0'` byte arithmetic. Both work
entirely in the NEGATIVE domain (`n` is never negated; digits accumulate as
`acc*10 - d`) so `i64::MIN`, which has no positive magnitude, formats and parses
without an overflow/trap.

A `string` is move-only, so building one cannot use a `loop` (Phase 11's accumulator
must be copyable): the buffer is threaded by MOVE through recursion, sound by Phases
4/6 conditional + recursive drop glue. The full native-completion stack supports the
new ops — type check + the six analyses (borrow/state/alloc/unsafe/escape/deps),
evaluator, source projection, typed-expr reconstruction, patch child-keys, three
`LoweredOp`s + lower fns + the lowered-IR verifier, native x86_64 + arm64 emit, and
the native IR validator + `verify`. The reference evaluator models a string as a
growable byte buffer (the native backend enforces the fixed capacity, an edge a
correctly-sized program never reaches); for correct programs eval == native. The
string-builtin evaluator bodies live in an `#[inline(never)]` helper so they do not
inflate the hot recursive eval frame and shrink the depth-before-overflow (the
documented eval GOTCHA from Phase 11). Round-trips, `verify`, and replay/export/import
all support it; the exported projection round-trips the new ops to a fixpoint.

Documented follow-on: the dynamic-buffer builtins (vec AND string) are not yet
`trace`-able — the tracer traps on them, a uniform pre-existing gap, not new to this
phase; read-only string helpers consume their arguments by move (a borrow-style API
would need shared-ref deref support inside the buffer builtins). `bytes<->string`
beyond `string_new` (static bytes -> string), and hex/unsigned formatting + a
write-to-buffer `fmt` variant, are left as straightforward stdlib follow-ons over the
same primitives.

Deliverables (delivered: the core surface; the noted variants are follow-ons):

```text
std.string: index, compare, concat, substring, push (bytes<->string: string_new only)
std.fmt: i64<->string (decimal; hex/unsigned + write-to-buffer are follow-ons)
```

Files touched:

```text
src/types.rs, src/expr.rs, src/lowering.rs, src/verify.rs, src/backend/native.rs,
  src/patch.rs (the three new primitives, full native-completion stack)
std/string.cdb, std/fmt.cdb (new)
tests/string_native.rs (new), tests/fmt_native.rs (new), tests/leak_interposer.rs
```

Acceptance fixture and oracle (met):

```text
a native program concatenates, compares, and indexes strings; format/parse
  round-trips i64 over a range including negatives, with no hand-rolled digit table
```

`tests/string_native.rs`'s `acceptance` program does all four in one native binary
(concat == "foobar", index byte 3 == 'b', compare apple<banana, and a -1234567
round-trip), with eval == native; its first test pins the new `string_with_capacity`/
`string_push`/`string_get` ops in the lowered IR. `tests/fmt_native.rs` pins the
round-trip natively across 0 / positive / negative / i64::MAX / i64::MIN and asserts
the exact formatted bytes (length and digit codes), not only invertibility.
`tests/leak_interposer.rs::string_build_frees_every_buffer_at_runtime` confirms the
no-leak half of exactly-once at runtime scale (every `string_with_capacity` and moved
`string_new` buffer is freed; net alloc - free is invariant to the allocation count).

## Phase 13 — Array Fill / Repeat Initializer (R9)

Goal: `[value; N]` so large fixed buffers are expressible as values.

Status: implemented. Resolves R9. `[value; count]` is a new `RawExpr::ArrayFill` /
typed `array_fill` node — NOT a parse-time desugar to `count` copies, because the
`.cdb` projection (a checked view) must round-trip the `[value; count]` form and the
value must be evaluated exactly ONCE. `value` is evaluated once and replicated into
all `count` slots of an `array<T, count>`; `count` is a non-negative integer literal
(the array size is a compile-time constant). The value must be a non-reference Copy
type with trivial drop (replicating a reference would duplicate a loan into every
slot; a move-only value would mint `count` owners) — the same discipline as the
dynamic-buffer element rule, which keeps the array-fill borrow/move analyses trivial:
each just recurses into the single value (no per-slot loan/move attribution).

Lowering evaluates the value once, then stores the (Copy) result into each slot,
reusing the existing `AddrOfIndex` + `Store` machinery — so there is NO new lowered op
and NO new native backend codegen (the fill is a per-slot store sequence). The lowered
IR is one store per slot; `[0; 1024]` type-checks and lowers (its ~8 KB stack frame
exceeds the v0 backend's frame limit, so large fills are gated at "lowers" per the
plan, while in-frame fills compile and run native). The evaluator and tracer replicate
the Copy result; eval == native. Round-trips, `verify`, and replay/export/import all
support it; the projection round-trips `[value; count]` to a fixpoint.

Deliverables (delivered):

```text
`[expr; N]` parsing, type rules, and lowering to a per-slot fill over the array place
```

Files touched:

```text
src/expr.rs (RawExpr::ArrayFill + parser + eval + projection + reconstruction + deps),
  src/types.rs (type rule + the 9 expr analyses), src/lowering.rs (fill lowering),
  src/trace.rs, src/patch.rs, src/bundle.rs, src/migrations.rs, src/lib.rs,
  src/backend_c.rs (emit-c bails, as it already does for array_literal)
tests/array_fill_native.rs (new)
```

Acceptance fixture and oracle (met):

```text
`[0; 1024]` type-checks and lowers; http_server.cdb uses a stack array buffer
  instead of malloc
```

`tests/array_fill_native.rs` pins `[0; 1024]` lowering (a store per slot, the value
lowered exactly once), in-frame `[7; 4]` / `[3; 8]` / a Copy-record `[{x,y}; 3]` /
`[42; 1]` running native (eval == native), the `[value; count]` projection round-trip,
and four fail-closed rejections (move-only value, reference value, zero count,
non-literal count). A stack-array buffer is now expressible as a value, so the
http_server-style buffer no longer needs `malloc` (the dedicated example remains a
follow-on).

## Phase 14 — Generics / Parametric Types (R11)

Goal: type parameters on fn/record/enum with monomorphization at lowering — the
one large rock the compiler genuinely needs (`Vec<T>`, `Option<T>`, `Result`).

Status: implemented for records, enums, AND functions — including recursive and
mutually-recursive generic functions (via a generic recursion group; see the final
paragraph). Resolves R11 for parametric types and parametric functions. Type
parameters on `record`/`enum` (`record Pair<T>`, `enum Option<T>`)
are constraint-free and monomorphized by **on-demand substitution** rather than a
stored-instance pass: a generic instance `Option<i64>` is the content hash of a
`Named` Type object carrying its type arguments (`{type_symbol: <generic>,
type_args: [i64]}`) — that hash *is* the instance's stable derived identity — and
its concrete structure is materialized by substituting the arguments into the
generic's template wherever structure is needed (`type_spec_in_root`, layout,
lowering). So instances are never separate stored objects, "monomorphize at
lowering" holds for layout/codegen, and import→export→import is a trivial fixpoint
(only the generic templates and their uses are projected — `enum Option<T>` and
`Option<i64>` — never the instances).

Representation: `TypeSpec`/`ParsedTypeSpec` gain a `TypeParam { index }` variant
(a positional, name-independent type-parameter reference — the opaque type during
generic-body checking, which is exactly constraint-free parametricity: arithmetic
or field access on a `T` fails) and a `type_args` field on `Named` (skipped when
empty, so every pre-generics Type-object hash is byte-identical). `TypeDefinition`
and the `RecordDef`/`EnumDef`/`CreateType` payloads gain `type_params` (also
skip-if-empty). A localized `bind_type_params` rewrite turns each `T` in a generic
definition's members into a `TypeParam` before resolution, so the rest of the
resolver needs no threaded scope; `substitute_type_hash`/`put_substituted_type`
(the type-arg twins of the existing region-substitution machinery) do the
substitution, with `materialize_named_type_expansion` transitively storing every
nested instance (`List<i64>`'s `box<List<i64>>`) so layout/lowering can load them
(a `seen` set + the `box` size-break keep recursive generics terminating).

Construction infers type arguments: `Option::some(5)` matches the variant's payload
template (`some: T`) against the payload's type to solve `T = i64`; `Option::none`
takes its argument from the expected type. The construction projects as the bare
`Option::some(..)` (no `<...>` at `::`) and re-infers identically on re-import, so
the grammar stays simple and the round trip is byte-stable. Layout (substitute
then lay out), the reference evaluator, native x86_64+arm64, `verify`
(`type_check_root` re-runs the arity + `TypeParam`-scope checks over every
instance; the object canonical-hash check validates the new `TypeParam`/`type_args`
payload forms), provenance (a generic's birth `create_type` records its
`type_params`, so blame on the generic identifies the parameters its instances
derive from), `trace`, and replay/export/import all support it. Wrong type-arg
arity, a bare generic in a type position, and applying a type parameter
(`T<i64>`, higher-kinded) all fail closed.

Deliverables (records, enums, and functions all delivered):

```text
type parameters on records, enums, and functions (constraint-free) — DONE
monomorphization (types: on-demand substitution; functions: instances materialized
  at the lowering seam, "at lowering" for layout/codegen) — DONE
stable derived identity for each instance (a type instance is the Named-with-
  type_args Type hash; a function instance is the hash of a descriptor naming its
  generic + type arguments) — DONE
verify recomputes and validates instances; provenance traces instance -> generic — DONE
```

Files touched:

```text
src/types.rs (TypeParam + Named.type_args, parser, bind_type_params, substitution,
  resolution, layout-feeding expansion, projection, enum-construct inference, the
  ~exhaustive-match fail-closed arms; AND for functions: signature type_params +
  reader, generic-call inference `type_generic_call` with the deferred-argument
  retry, the monomorphization pass `monomorphize_into_root` + typed-expression
  substitution walker `substitute_typed_expr`, `value_class_in_root` parametric
  class, the two call verifiers' type-arg substitution, instance verify),
  src/migrations.rs (CreateType/CreateFunction.type_params, apply + source-round-trip
  postconditions, monomorphize at apply + recursion-group member bodies),
  src/layout.rs, src/lowering.rs (generic-call → instance target), src/verify.rs
  (generic-instance consistency + reference check), src/provenance.rs, src/diff.rs
  (skip unnamed derived symbols), src/expr.rs (TypeDefinitionSource/FunctionSource
  .type_params, def + fn header `<T>` parse + projection, named-dependency
  projection ordering, eval `TypeParam` value-typing), src/api.rs, src/lib.rs
  (importer + recursive-generic-function fail-closed)
tests/generics_native.rs (new)
```

Acceptance fixture and oracle (met):

```text
one generic Option<T> compiles natively at two instantiations (Option<i64>,
  Option<bool>), eval == native; blame on the generic Pair records its type
  parameters; one generic function id<T> compiles natively at two instantiations
  (id<i64>, id<bool>), eval == native, each a distinct native symbol; blame on the
  generic id records its type parameters, tracing every instance back to it
```

`tests/generics_native.rs` pins the type fixtures (generic `Option<T>` at
`Option<i64>`+`Option<bool>` native; a generic `record Pair<T>`; distinct-layout
`Boxed<i64>` vs `Boxed<bool>`; nested `Option<Pair<i64>>`; the instance→generic
provenance trace; the import→export→import fixpoint) AND the generic-function
fixtures: `id<T>` at i64+bool native (eval == native), a generic function over a
generic enum (`unwrap_or<T>(Option<T>, T)` — with `Option::none` resolved by the
deferred-argument retry), a generic function over a generic record (`make<T> ->
Pair<T>` feeding `first_of<T>(Pair<T>)`), distinct monomorphizations with distinct
layouts (`tag_of<i64>` vs `tag_of<bool>`), blame recording the function's type
params, the generic-function import→export→import fixpoint with a byte-stable
projection (instances never projected), `verify` rejecting an instance inconsistent
with its generic, and fail-closed rejections (arity, higher-kinded `T<i64>`,
un-inferrable type arguments).

Generic functions: representation and monomorphization. A function's signature
carries `type_params` (skip-if-empty, so a non-generic signature hashes
identically); its parameter/return types use `TypeParam { index }` and it is
type-checked once with the parameters opaque (`value_class_in_root` gives any
`TypeParam`-bearing type the conservative move-only/needs-drop parametric class, so
the template borrow/move/drop check is sound for every instantiation). A generic
call records the inferred `type_args` on the call expression (so the reference
evaluator runs the type-erased generic body unchanged, while the native backend
sees the instantiation). Inference matches the argument types against the parameter
templates with `infer_type_args_from_match` (shared with enum construction), falls
back to the expected result type, and retries an argument that could not type on
its own once `T` is solved from its siblings. **Monomorphization happens at the
lowering seam**: after a body type-checks, `monomorphize_into_root` walks it,
materializes each concrete `(generic, type_args)` instantiation as a derived,
*unnamed* root symbol (a substituted concrete signature + a type-substituted
concrete body), and recurses (a generic calling a generic). An instance's stable
symbol is the content hash of a `MonomorphicFunctionInstance` descriptor naming the
generic and its type arguments — so its native ABI symbol is distinct per
instantiation, two call sites at the same type share one instance, and re-import
reproduces it. Reachability and lowering map a generic call to its instance symbol;
projection ordering maps the instance back to the named generic (so a callee is
emitted before its caller). Because instances are unnamed and derived
deterministically, the projection emits only the generic templates and bare calls —
never the instances — and import→export→import is a fixpoint.

Recursive and mutually-recursive generic functions are supported through a *generic
recursion group* — the Phase 5 recursion-group object whose members now carry
`type_params` (skip-if-empty, so a non-generic clique's `CreateRecursionGroup` op
and every existing clique hash is byte-identical). The clique binds its members'
generic signatures (`<T>`) before any body is type-checked, so a member may call
itself and its peers generically (each in-group call infers `[TypeParam{index}]`
type arguments); the concrete instances are monomorphized at the lowering seam by
the same worklist, which co-materializes a mutually-recursive instance pair and
terminates on the back-edge (the instance's recursive call re-enters an
already-built instance). The instances are ordinary unnamed derived symbols,
recursive by-symbol, so lowering, the at-most-once verifier, intra-procedural
effect/borrow checks (satisfied inductively over the clique), and visited-set
reachability all carry over from non-generic recursion unchanged. One projection
subtlety the recursion group forces: the source ordering follows a call's *named*
callee (`collect_named_call_symbols`), not its monomorphic instance, because a
clique member's in-group calls are at `TypeParam` arguments whose instance does not
exist — so a non-clique function projected alongside the clique keeps its parse
position and the import→export→import root hash stays a fixpoint. Blame on a
recursive generic function reports the `create_recursion_group` migration that
records each member's type parameters. (A recursive generic function over a
*mutually-recursive generic type* clique — a generic `CreateTypeGroup`, D1 — remains
a follow-on (it fails closed: the clique's `<T>` is not yet threaded through
`CreateTypeGroup`); a self-recursive generic function over a single generic type, and
a recursive generic threading a generic-typed value, are supported.)

## Phase 15 — Self-Hosted Front-End to Lowered IR (Ladder Rung A)

Goal: express the front half of the compiler as CodeDB objects and meet the Rust
native backend at the lowered-IR seam — the mixed compiler.

Status: in progress — sub-stage 15a.1 (the lexer probe) is landed; 15a.2–15e are
planned. Self-hosts rung A. Depends on Phases 6, 7, 9–14 (all complete) and the
Phase 8 CIR artifact (rung A produces the CIR that rung 0 consumes — the two meet
at the same flat binary).

Landed substrate (15a.0): two determinism-oracle references for the front-end.
`emit-objects <db> --out` (`CodeDb::export_objects_branch`) dumps the object
closure of a branch root as `<hash>\t<kind>\t<schema_version>\t<canonical_payload>`
lines sorted by hash plus a trailing `root <hash>` pin — the canonical bytes the
self-hosted importer must reproduce and the divergence localizer for the
root-hash oracle (the closure walk already proves every payload canonical and
re-hashing to its own hash). `codedb::token_probe(source)` / `emit-tokens <file>`
is the lexer reference: `tokens <count> fnv32 <digest>`, the FNV-1a-32 over each
token's kind byte then its text bytes.

Landed 15a.1: `compiler/front/lex.cdb` — the first self-hosted front-end object —
reads source bytes from stdin (the Phase 8 1-byte-bounce-buffer pump), tokenizes
them exactly like `src/expr.rs::lex` over a move-only memory string (the Phase 8
threading discipline; every byte read guarded by `if p < len` because the
language's `&&` is strict, evaluating both operands), and prints the same probe.
It compiles native and its probe is byte-equal to `token_probe` on a varied corpus
(idents with underscores/digits, decimal + `0x` hex, `//` comments, all ten
two-char symbols, whitespace, a recursive multi-line program, and `"`/`b"` string +
byte-string literals folded over their DECODED bytes — `\n`/`\t`/`\"`/`\\` and the
byte-string `\0`/`\xHH`) AND on the **entire committed corpus** token-for-token:
all of `std/*`, `examples/v3/{tokenizer,sha256}.cdb`, the 1700-line
`compiler/eval/eval.cdb`, and the lexer tokenizing itself. The committed source
passes the §11 checked-view gate (import→export→import fixpoint, byte-stable
projection). String literals fold the decoded value, not the raw slice (mirroring
`lex_string`/`lex_byte_string`); the only assumption is ASCII outside string/comment
content (every committed source satisfies it — `\`/`"` never occur as UTF-8
continuation bytes, so byte-level escape/quote detection and the re-encoded decoded
bytes match the Rust `char` walk exactly). Two .cdb-authoring realities resurfaced
and are pinned in the source: record literals in `if`/`case` branch and
function-return position must be bound to a typed `let` (else they take a
structural, field-sorted layout that mismatches the nominal record — the Phase 8
gotcha), and the per-token work is split into `classify`/`step` routers so each
frame stays under the v0 4095-byte budget.

Landed keystone (15a.3, the content-addressing core): `compiler/front/sha256.cdb`
is **general multi-block SHA-256** of arbitrary stdin bytes → lowercase hex,
byte-equal to `codedb::sha256_hex` on empty input, every SHA-256 padding edge
(55/56/63/64/65/127/128…), multi-block messages, all 256 byte values, and a
canonical-JSON-payload-shaped input. SPEC_V3 §5 names this the rung-A gate ("the
importer cannot self-host until the language can compute SHA-256"): the existing
`examples/v3/sha256.cdb` proved only the single fixed "abc" block, so this file
reuses its compression core verbatim and adds arbitrary ingestion, spec padding
(0x80 + zero-fill to 56 mod 64 + 64-bit BE length), multi-block state chaining
(`compress_from` threads the running eight-word state), big-endian word reads
(inlined so `string_get` borrows the owned buffer rather than a helper consuming
it), and hex output. `hash_object_canonical` is exactly this over the
domain-framed object preimage (`OBJECT_DOMAIN || kind || \0 || schema || \0 ||
payload`). The object-hash wrapper is now landed too: the `obj_hash` entry of
`compiler/front/sha256.cdb` reads `kind\nschema\npayload` from stdin, frames the
domain-prefixed preimage (rewriting the two newlines to `\0` and prepending
`OBJECT_DOMAIN`), SHA-256s it, and prints `sha256:`+hex — reproducing
`src/store.rs::hash_object_canonical` exactly. Its oracle is `emit-objects`
itself: every dump line is a real `(kind, schema, canonical payload → hash)` case,
so the `.cdb` provably computes the SAME object hashes CodeDB does, across a
record, an enum, and several functions (short and long payloads). The
content-addressing core — raw SHA-256 and the object framing — fully self-hosts.

Landed milestone (15a.3, minimal grammar): **root-hash equality**.
`compiler/front/import.cdb` reads `fn main() -> i64 = <int>` from stdin, builds the
six content-addressed objects (Type i64, Expression, FunctionSignature,
SymbolBirth, FunctionDef, ProgramRoot) with their EXACT canonical-JSON payloads in
dependency order, chains `hash_object` over them (reusing the SHA-256 core; reused
hashes — the i64 Type, the Signature, the SymbolBirth — are threaded back from the
build helpers, since a `.cdb` helper taking a `string` by value drops it and
`string_get` rejects `&string`), and emits the ProgramRoot hash. That root EQUALS
the Rust importer's (`codedb import → root …`) for every value tested (0, 1, 42,
1000000, i64::MAX) — the `.cdb` computes the same program identity CodeDB does, with
the deterministic birth identity reproduced exactly (the SymbolBirth `local_nonce`
is the literal `import:main:main:0` = `import:<module>:<name>:<ordinal>`). The
committed source passes the §11 checked-view gate. The "parser" here is minimal (it
extracts the decimal int after `=`); the real tokenize+parse and the other object
kinds are what GROWING the grammar adds.

`tests/selfhost_frontend.rs` is the gate (7 tests: lexer × full corpus, the §11
checked-view gate, emit-objects determinism, SHA-256 × lengths/blocks, obj_hash ×
real objects, and importer root-hash equality × int values). Next: grow the importer
grammar (15a.2+) toward full-corpus root-hash equality, then 15b–15e (typecheck → …
→ lowering → CIR) for the mixed compiler.

Continuation plan (de-risking spikes, 2026-06-14). Two empirical spikes against the
Rust importer settled the shape of the remaining importer work and split it into two
orthogonal axes — only one of which the one-symbol minimal grammar has paid:

```text
axis 1 — grammar width   a single NON-recursive function: lex -> parse -> typecheck
                         -> build the TYPED objects. birth is genesis, so there is no
                         chain; testable per-object against emit-objects lines.
axis 2 — chain depth     2+ items: canonical item ordering (Tarjan SCC + Kahn
                         toposort + alphabetical tie-break) + the migration/history
                         hash chain that seeds every non-first symbol's birth.
```

The chain mechanism the importer must reproduce for any multi-item program (and the
reason the one-symbol minimal grammar needed none of it):

```text
migration_hash = sha256(MIGRATION_DOMAIN || canonical_json{parent_history_hash,
                   input_root_hash, output_root_hash, operation, preconditions,
                   postconditions})
history_hash   = sha256(HISTORY_DOMAIN || parent_history || \0 || migration_hash
                   || \0 || output_root)
birth_history_hash(symbol) = the running history_hash at its creation ("genesis" for
                   the first); local_nonce = import:<module>:<name>:<item_ordinal>
```

Three findings fix the build order:

```text
- preconditions/postconditions are a FIXED template, mechanically derivable from the
  operation: [root_is_current(in_root), name_is_available(mod, name)] and
  [root_exists(out_root), function_source_matches(<operation minus birth_seed/kind>)].
  So the chain is string-templating, like the importer already does for payloads.
- DUAL serialization. The operation body (which feeds migration_hash) is the RAW
  source AST (param_name, types as strings, kind: let/if/unary/binary/call). The
  Expression OBJECTS are the TYPED form (param_ref by index, local_ref by de-Bruijn
  depth, type hashes, content-addressed children, == inferring Bool). Multi-item
  root-hash equality needs BOTH serializers; one symbol needs only the typed one.
- self-recursion — even a single fn calling itself — emits create_recursion_group
  with member seed recursion_group:<ordinal>, NOT create_function. Recursion is
  therefore axis-2 work (recursion-group + canonical member ordinals); axis-1 stays
  on non-recursive single functions.
```

Recommended sequence (steps 2–4 all widen sub-stage 15a):

```text
step 1  shared compiler/front/lib.cdb: dedup the SHA-256 core + object framing +
        canonical-json/string helpers, imported first (the multi-file bootstrap the
        lexer test already uses) — the substrate for every later step.
step 2  15a.2 (axis 1): real lexer -> parser -> typechecker -> typed-object builder
        for a single non-recursive fn. oracle: per-object hash, then root-hash
        equality. widen the body in order: binary -> let/if/unary -> sized/bool/hex
        literals + the inference they force -> params/param_ref -> no-new-symbol
        builtins.
step 3  15a.4 (axis 2): the migration/history chain on a trivial multi-fn grammar
        (raw-AST serializer + the pre/post templates + migration_hash + apply-to-root
        + history chaining + canonical item ordering). oracle: multi-fn root equality.
step 4  object-kind breadth: calls (cross-symbol), records, enums, externs, recursion
        groups (canonical member ordinals, reusing Phase 5), generics.
step 5  15b–15e: extend through lowering to CIR-byte equality — the mixed compiler,
        meeting the Phase 8 rung-0 evaluator at the same flat binary.
```

Landed 2026-06-14: step 1 is done. `compiler/front/lib.cdb` holds the shared SHA-256
core + object framing + hex/stdin plumbing, imported first; `sha256.cdb` and
`import.cdb` are slimmed to their unique parts and resolve it cross-file — proving the
multi-file composition the later stages (15b–15e) depend on: root-module functions,
record types, and externs all resolve across imported files, and dead lib functions
drop from each entry's build. `tests/selfhost_frontend.rs` stays green (7/7).

Landed 2026-06-14: step 2 (15a.2) — the real expression parser. `import.cdb` replaces
the int-extraction hack with a scannerless recursive-descent parser for `fn main() ->
i64 = <expr>` over the full i64 operator set: prefix unary `- ~`, then the precedence
ladder `bitor | -> bitxor ^ -> bitand & -> shift << >> -> add + - -> mul * / % -> unary
-> atom` (Rust precedence, left-associative; `<<`/`>>` distinguished from lone `<`/`>`
and `&`/`|` from `&&`/`||`, the bool forms being a later increment). Each parse function
returns the content hash of the typed Expression object it just built (children by hash),
so no AST data structure is needed and objects emit in dependency order. Single function,
genesis birth — still axis 1, no chain. Root-hash equality holds across 33 integer-
expression fixtures (precedence, associativity, unary, shifts, bitwise) plus the 6 literal
ones (`tests/selfhost_frontend.rs`, 8/8). Surfaced a v0-backend codegen bug en route,
worked around in `.cdb` and flagged for a proper fix:

```text
v0-backend bug (native-only): a function that returns a move-only LOOP ACCUMULATOR
  directly (`... in done`) miscompiles to a SIGTRAP when the loop body calls an
  aggregate-returning function — a loop-accumulator / return-slot aliasing bug.
  Workaround: rebuild the result into a fresh record before returning
  (`let r: Pr = { ..done fields.. } in r`). The reference evaluator computes both
  forms identically, so it is a backend codegen defect, not a language-semantics one;
  a minimal Rust-side repro + lowering/backend fix is a follow-on (it will recur as the
  parser grows more loop-returning functions, so the workaround is load-bearing until
  fixed).
```

Landed 2026-06-14: step 2 (15a.2) gains TYPE INFERENCE. `import.cdb` now infers each
expression's type (i64 vs bool) and threads it through every parse result (`Pr.tyc`)
into the node's `type` field, and reads the function's declared return type from the
header. New levels: equality (`== !=`) and relational (`< > <= >=`), which take i64
operands and yield bool, inserted between bitand and shift; plus a `parse_header` that
reads `-> i64` / `-> bool`. So `fn main() -> bool = <comparison>` builds the bool Type,
the bool-typed comparison node, and the bool-returning signature — all matching the Rust
importer. Root-hash equality holds across 17 comparison/bool-return fixtures plus the 33
integer + 6 literal ones (`tests/selfhost_frontend.rs`, 9/9). The parser deliberately
does NOT type-check (it builds the tree and infers result types), so the oracle is
defined only on type-valid programs — a `7 & (3==3)` (i64 & bool) is rejected by the Rust
importer and out of scope. Still single-function / genesis-birth (axis 1).

Then (same day) the bool surface was completed: bool literals `true`/`false` (atoms,
value a bare JSON boolean), logical `&&`/`||` (new logor/logand levels between bitor and
bitxor, bool-typed), and unary `!` (bool); `build_unary` gained a result-type code. 30
bool-expression fixtures now hold (comparisons, literals, `! && ||`, precedence cross-
checks like `(true && false) || true` and `1 < 2 && 3 < 4`), `tests/selfhost_frontend.rs`
9/9.

Then `if cond then a else b` landed — the structural step. Because cond/then/else are
full expressions, `parse_atom`/`parse_if` and the binary ladder become one mutual-
recursion clique, which codedb imports + compiles as a single recursion group (the
keystone Phase 5/6 machinery, now exercised at ~14 members from a real .cdb). The if
node's type is the then/else branch type, threaded like any other; root-hash equality
holds across 9 if fixtures (i64/bool results, logical/comparison conds, nested if,
else-if chains), `tests/selfhost_frontend.rs` 10/10. Known intentional over-permissiveness:
the parser treats `if` as an atom, so it accepts `1 + if ...` which the Rust grammar
rejects as a parse error — harmless for the oracle (valid programs only); matching Rust's
exact `if`-position rejection is a later grammar-tightening.

Then `let IDENT: TYPE = value in body` + identifier (local variable) references landed —
the scope-tracking step, completing the tractable axis-1 expression surface. An identifier
resolves to a `local_ref` by **de-Bruijn depth** (innermost binding = 0); the parser
threads a lexical scope down the ladder as a *Copy* input param (`Scope { ids: array<i64,
32>, n }`, each binding encoding its name's source byte-range + type code in one i64),
extended only into a `let` body and never returned — so lexical scope restores naturally
on exit. Resolution scans the scope keeping the highest (innermost) match, so a shadowing
binding resolves to the nearest one, and the ref is typed by the binding it names; the
let's own type is its body's type, the value is parsed in the outer scope and the body in
the extended scope. `let` joins `if` in the mutual-recursion clique (the .cdb still
imports + compiles as one recursion group, now ~15 members, plus a self-recursive decimal
renderer for the bare-integer `depth`). The de-Bruijn depth and binding/ref types were all
spiked against `emit-objects` before coding, never guessed. Root-hash equality holds across
17 fixtures — single/nested/three-level bindings, shadowing, a value referencing an outer
binding, bool bindings, lets nested with `if`, multi-character identifiers, and deep nests
whose depths span two decimal digits (also smoke-tested to the 32-entry scope capacity)
(`tests/selfhost_frontend.rs`, 11/11). Still single-function / genesis-birth (axis 1).
Remaining axis-1 surface: function params + `param_ref` (a separate index namespace);
then no-new-symbol builtins. Then 15a.4 (axis 2 — the migration/history chain for
multi-function programs).

Then function parameters + `param_ref` landed. The header now parses a parameter list
`(a: i64, b: bool, ...)`, which feeds the FunctionSignature's `params` (a list of Type
hashes, previously always empty) and the ProgramRoot's `param_names`; an in-body
reference to a parameter becomes a `param_ref` by its positional index, typed by that
parameter. The forms were spiked against `emit-objects` first (confirming `main` accepts
parameters, the `param_ref`/signature/param_names shapes, and — critically — that a
parameter reference inside nested lets is still a `param_ref`, a let shadows a same-named
parameter, and a let's de-Bruijn depth does not count parameters: lets and params are
separate namespaces). Implemented by widening the lexical scope to hold BOTH in one Copy
array — parameters at indices [0, np), `let` bindings pushed on top at [np, n), with np
threaded alongside — so resolution checks the let region first (`local_ref`, depth
n-1-idx) then the parameter region (`param_ref`, index idx). The parameter list is parsed
by a recursive `scan_params`; the signature param list and param_names are emitted by
recursive helpers (`push_sig_params_at` / `push_pnames_at`). Root-hash equality holds
across 17 fixtures — single/multiple/out-of-order params, mixed i64/bool param and return
types, params mixed with lets (shadowing, a value using params, a param referenced from
inside nested lets), and an 11-parameter case whose last `param_ref` index spans two
decimal digits (`tests/selfhost_frontend.rs`, 12/12). One backend reality forced a design
choice: the v0 arm64 backend sizes each aggregate stack local at its full byte width, so
threading two 32-element scope arrays (or even one, given the new param-scanning /
object-emitting functions that juggle several `Scope`-typed locals) overflowed the
4095-byte frame; folding params + lets into a SINGLE array and capping it at 12 combined
entries (ample for an axis-1 single function) brought every function back under budget.
Still single-function / genesis-birth (axis 1). Remaining axis-1 surface: no-new-symbol
builtins. Then 15a.4 (axis 2 — the migration/history chain for multi-function programs).

Then hex integer literals landed (a quick win): `parse_atom` recognizes a `0x`/`0X`
prefix and scans hex digits; the canonical `value` is the raw source slice (no
normalization), so it stays a `literal_i64` node.

Then sized integer types (u8/u16/u32/u64/i8/i16/i32) + the EXPECTATION PROPAGATION they
force. The importer was bottom-up only (literals defaulted to i64); reproducing the Rust
importer's sized-literal typing required a real top-down pass, so an expected type code
is now threaded down the parser alongside the scope. The rules (all spiked against
`emit-objects` first): an integer literal takes the expected type if set, else i64; a
binding/return/parameter annotation supplies it (`type_code_of` classifies the type
name); arithmetic/bitwise/shift propagate it to both operands; a comparison's operands
unify (left informs right) and yield bool; `if` is a propagation barrier (Rust does not
push the expectation into branches, so `-> u32 = if c then 1 else 2` is a type error and
out of scope); `let` value gets the binding type and body gets the outer expectation;
casts are fixed by the cast name (next increment). `type_hash` grew to nine `type_kind`s.
A subtle bug surfaced and was fixed: the lexical-scope entry packed `btyc` in one bit
(`*2`), fine for i64/bool but corrupting a sized binding/param's type and name range —
widened to four bits (`*16`). One documented gap: a bare literal LEFT of a concretely
typed operand with no outer expectation (`1 < a`) is not unified in the single pass (the
literal stays i64); such programs are out of scope (tighten when the importer grows real
unification). Root-hash equality holds across 27 sized fixtures — every width as a
return-driven literal, hex-in-sized, propagation through arithmetic/bitwise/shift, sized
params feeding arithmetic/unary, sized `let` bindings propagating into the body, and
uniform/concrete-left sized comparisons — plus the i64/bool regressions
(`tests/selfhost_frontend.rs` 14/14). Still single-function / genesis-birth (axis 1).
Remaining axis-1 surface: cast builtins (`to_u8`/… → `int_cast`), then no-new-symbol
builtins generally. Then 15a.4 (axis 2 — the migration/history chain for multi-function
programs).

Then the integer cast builtins landed (`to_u8`/`to_u16`/…/`to_i64`). In `parse_atom`,
an identifier that begins `to_` and is immediately followed by `(` is a cast: the target
type is named by the suffix, the argument is a full expression parsed with NO expectation
(so it keeps its own type, recorded as `source_type`), and the result type is the target
regardless of the surrounding expectation. Builds an `int_cast{source_type, type, value}`
Expression (forms spiked against `emit-objects`). The keyword/cast classification was
factored into a `classify_kw` helper — its byte-test temporaries were inflating
`parse_atom`'s stack frame past the v0 4095-byte budget (a build failure caught in
smoke-testing); moving them into a helper frame fixed it. Root-hash equality holds across
18 cast fixtures — every target width, a hex and an arithmetic argument, casts as
operands and in `let` bindings, parameter arguments (source_type = the parameter's type),
and nested casts — plus regressions (`tests/selfhost_frontend.rs` 14/14). This completes
the tractable axis-1 grammar for a single non-recursive function. Next: 15a.4 (axis 2 —
the migration/history chain for multi-function programs; the spikes have already mapped
the per-function `create_function` migration, the `birth_history_hash` chain, the raw-AST
operation body, and the incremental output root).

Landed 2026-06-15: 15a.4 (axis 2) KEYSTONE — the migration/history chain for a TWO-FUNCTION
program. `import_root` now detects a second `fn` (parse the first body, skip_ws past its
end; EOF ⇒ the unchanged single-function path, else the two-function path) and builds the
ROOT through the real chain: order the two functions canonically (alphabetical by name via
`name_le`), build the FIRST function's single-symbol ProgramRoot (out1), fold it through
the first migration into the running history (hh1), then build the final two-symbol
ProgramRoot with the second symbol born at hh1. The migration is the dual/raw serialization:
`hash_domain` (a kind/schema-less `"sha256:"+hex(SHA-256(domain||payload))` — added
alongside the object hasher) over the canonical migration JSON (operation = create_function
with a RAW-AST body `{kind:literal_i64,value}`, plus root_is_current/name_is_available
preconditions and root_exists/function_source_matches postconditions, MIGRATION_DOMAIN),
then `history_hash` = hash_domain(HISTORY_DOMAIN, ""+\0+migration_hash+\0+out1); the first
symbol's SymbolBirth carries the literal `"genesis"`, the second carries hh1. **Subtlety
caught in smoke (the keystone's whole point):** a ProgramRoot's `names` array is ordered by
display-name but its `param_names`/`symbols` arrays are ordered by SYMBOL HASH
(`src/model.rs` normalize_root) — the two coincide only when symbol-hash order matches name
order, so `helper=5,main=5` (helper symbol 4ac4… > main symbol 3c4d…) diverged until
`build_root2` compared the symbols (`sym_le`) and emitted those two arrays in hash order.
Root-hash equality is exact and holds across 6 fixtures — source order ==/!= canonical
order, both-non-main names, assorted/equal values — plus all single-function regressions
(`tests/selfhost_frontend.rs` 15/15). Whole-spec de-risked in Python first (every object
hash + the chain reproduced byte-exactly before any `.cdb`), and the v0 frame budget forced
heavy factoring (the migration-payload and two-symbol-ProgramRoot builders each split in two
via carrier records; parse_header's three scan loops factored into `find_byte`/
`skip_to_arrow`; frames validated OFFLINE via `emit-ir` + a backend-formula replica before
each slow verify+build). Keystone scope: two no-param i64-returning functions with literal
bodies; growing the raw-AST serializer (operators/let/if/refs/casts), cross-symbol calls,
params-in-multi-fn, and recursion groups (`create_recursion_group`) is the remaining axis-2
surface.

Landed 2026-06-15: the axis-2 raw-AST serializer grew to the FULL single-function grammar,
so a two-function program's bodies may now be any expression, not just a literal. A
multi-function program's migration body is the dual (RAW) serialization of the canonical-
first function's body — distinct from the typed Expression objects (which reference children
by content hash) — so reproducing the root for a first body that is any expression requires a
byte-exact raw AST. A SECOND, type-free recursive-descent parser now produces it, mirroring
the typed precedence ladder exactly (so the raw tree matches the typed tree) but far simpler:
no expected type, no scope, no type codes; sized and hex literals all serialize as
`literal_i64` with the raw source text, refs (let-locals or params) are `param_name` by name,
and `to_*` casts are `call` nodes. It is its own mutual-recursion clique (a second recursion
group alongside the typed parser's). The chain builders embed the raw body in BOTH the
operation and the function_source_matches postcondition and emit the function's real
return-type NAME (no longer hard-coded `i64`); the two-function path re-parses each body on
demand — typed (for the Expression hash) and raw (for the migration) — so `Sl` now carries the
body's source start plus the return-type code instead of a literal value span, and
`build_fn_objs` re-derives the typed body via `parse_bitor`. Because a raw node embeds its
children INLINE (unlike a typed node's fixed-length hash children), each node-builder sizes its
buffer DYNAMICALLY from the child lengths — a fixed `string_with_capacity` would trap on deep
nesting (and the migration payload, which holds the body twice, is sized from it too). Every
raw form was spiked against `codedb history --json` first (binary/unary/if canonical key
orders, `param_name` refs, `call` casts, bare-bool and raw-text literals) and the whole rich
chain reproduced byte-exactly in Python before any `.cdb`; all 112 functions were validated
under the v0 frame budget OFFLINE (`emit-ir` + the backend-formula replica) before the slow
verify. Root-hash equality holds across 22 rich two-function fixtures — operators/precedence/
associativity, bitwise/shift, comparison, logical, unary, bool, nested `if`, nested `let` +
by-name refs, casts, hex, sized types, a rich SECOND body (the typed re-parse path), both
rich, and canonical ordering with the rich body sorting second — plus all single-function and
literal-keystone regressions (`tests/selfhost_frontend.rs` 16/16). Remaining axis-2 surface:
cross-symbol calls, params-in-multi-function (param refs in a multi-fn body), and recursion
groups (`create_recursion_group`).

Landed 2026-06-15: no-argument CROSS-SYMBOL calls in a two-function program — the first
dependency-driven increment. A call (`<callee>()`) introduces a dependency edge, so the
canonical order is no longer alphabetical but a TOPOSORT: the callee is created before its
caller (ordinal 0/genesis vs ordinal 1/running-history), so the caller's typed body can
reference the callee's already-determined symbol. `classify_kw` now distinguishes an identifier
immediately followed by `(` as a user call (vs a `to_*` cast); the typed `parse_call` builds a
`call` Expression `{args:[],expr_kind:call,symbol,type}` that references the callee by
content-addressed symbol HASH (re-derived directly from the call name — in a two-function DAG
the callee is always the canonical-first, born at genesis/0 — so no symbol table is threaded
through the parser), typed by the callee's return type found by re-scanning the source headers
(`find_callee_rtc`); the raw parser emits the matching `call` node (unexercised in two-function
DAGs, where the canonical-first never calls, but kept for completeness). `import_root` detects
the call edges (`body_calls`) and orders callee-first, falling back to alphabetical for
independent functions (mutual recursion is a recursion group — out of scope). Every form was
spiked against `codedb history --json` and the call-program root reproduced byte-exactly in
Python before any `.cdb`. The keystone subtlety this increment EXPOSED (caught only in smoke,
the whole point of it): a ProgramRoot's `names` array is display-name-ordered while its
`param_names`/`symbols` arrays are symbol-hash-ordered — the two coincided for every prior test
because canonical order was always alphabetical, but a call makes toposort ≠ alphabetical, so
`build_root2` now orders the `names` array independently (reusing `order_triples` to put the
(sym, def, sig) triples in display order for `names`, then re-ordering the same triples by
symbol hash for the tail). Two v0-backend realities recurred: the names emission had to stay
out of a record-returning conditional (string-building inside one miscompiles to a SIGTRAP — so
the conditional only shuffles data and the emission is unconditional, the `order_triples` +
`build_root2_tail` shape), and it was factored into `build_root2_names` to fit the 4095-byte
frame. Root-hash equality holds across 11 cross-symbol-call fixtures — dependency order matching
AND contradicting alphabetical, the call in arithmetic/`if`/`let`, a bool-returning callee, and a
callee with a non-literal body (its migration body exercising the raw serializer) — plus all
regressions (`tests/selfhost_frontend.rs` 17/17). Remaining axis-2 surface: arguments to calls
and params-in-multi-function (param refs in a multi-fn body), and recursion groups
(`create_recursion_group` — self-recursive or mutually-recursive functions).

Landed 2026-06-15: SELF-RECURSIVE functions — a single function whose body calls its own name
is a one-member RECURSION GROUP, the first `create_recursion_group` increment. The importer
emits `create_recursion_group` rather than `create_function`: the member's SymbolBirth uses
`local_nonce = "recursion_group:0"` (the member's ordinal in canonical order, NOT its name); a
new `RecursionGroup` object (`{"members":[{definition,signature,symbol}],"module":...}`) is
created; and the ProgramRoot carries a `recursion_groups` array (key order metadata, names,
param_names, recursion_groups, symbols) alongside `symbols`. The self-call resolves to that
`recursion_group:0` symbol: the lexical `Scope` (already threaded through the parser) gained an
`rgord` field (-1 = not a recursion-group member; >= 0 = this body is a member of that ordinal),
and `parse_call` resolves a call through `resolve_call_symbol` — `rgord >= 0` gives the
`recursion_group:<rgord>` symbol, otherwise the create_function symbol named by the call.
`import_root` detects self-recursion (`body_calls` on the function's own name) and, on the
single-function path, builds the recursion-group root (`build_rg_root`, which re-parses the body
with `rgord = 0`) instead of `build_root_obj`. Because the member's birth is `genesis`, the root
needs NO migration/history chain (simpler than the multi-function case) — it is reproduced from
objects alone. Every shape was spiked against `codedb history --json` / `emit-objects` and the
root reproduced byte-exactly in Python first. The symbol resolution was kept off the v0
SIGTRAP path (`resolve_call_symbol` is a conditional of two whole symbol-builder calls, no
string-building inside the `if`), and `import_root` — which overflowed the 4095-byte frame once
the self-recursion detection joined the two-function toposort — was factored (the two-function
branch moved into `import_two`). Root-hash equality holds across 8 self-recursion fixtures —
bare self-call, base case, self-call in arithmetic / `let`, two self-calls, a bool result, and
distinct names — plus all regressions (`tests/selfhost_frontend.rs` 18/18). Remaining axis-2
surface: MUTUAL recursion (a multi-member recursion group, needing the canonical member ordering
— Tarjan SCC), and call arguments / params-in-multi-function.

Landed 2026-06-15: MUTUAL RECURSION — two functions that call each other are a TWO-member
recursion group, the keystone surprise being that a clique's member order is NOT alphabetical but
the Rust importer's `canonical_clique_order` (src/lib.rs): a 2-round Weisfeiler–Leman colour
refinement reproduced in the `.cdb`. Each member's colour is `sha256("codedb/recursion-order/v1\0"
| static_sig | erased_body)`, where the static sig is `{"effects":[],"params":[],"regions":[],
"return":"<type>"}` and the erased body is the member's raw AST with every peer call `<name>`
rewritten to `@recursion-peer:<peer's colour>`. Round 1 uses empty peer colours; if the two colours
are equal the clique is symmetric (automorphic) and the order is the alphabetical name tie-break,
otherwise round 2 recolours with the round-1 colours and the members sort by the converged colour.
A blueprint proved round-1-only is wrong (110/208 synthetic asymmetric pairs disagree with round 2)
— round 2 is mandatory. New `.cdb`: a recursive `recolor` (a loop form SIGTRAPs — the v0
loop-accumulator-return bug), Copy `Color` bytes (branch-free colour selection + free threading),
`member_color`/`order_clique`/`order_round2`. Because clique_rank gives the ALPHABETICAL rank but
the symbol ordinal is the WL order, `Scope` gained `aord` (the alpha-first member's WL ordinal) and
`parse_call` maps rank→ordinal through it. The ProgramRoot's `names` array is display-name-ordered
while param_names/symbols are symbol-hash- (= ordinal-) ordered — these DIVERGE for asymmetric
cliques (the same keystone as cross-symbol calls), handled by ordering the triples in display order
for `names` (`build_mutrec_root_obj` → `build_mutrec_names`, mirroring `build_root2`) and by hash for
the tail. Two non-obvious native SIGTRAPs were isolated by probe entries + Python blueprint: a
loop-form recolor (→ recursion), and `member_color` overflowing `sig`'s fixed-capacity buffer when
appending `| erased` (`string_with_capacity` traps past capacity — the payload now uses a fresh
dynamically-sized buffer, and recolor's output buffer is sized `rawlen*4` for the round-2 colour
expansion). Both members' births are genesis (one `create_recursion_group` migration), so the root
needs no migration/history chain. NO Rust changed. Root-hash equality holds across 23 mutual-
recursion fixtures — bare cliques, longer/bool/digit names, both source orders, AND asymmetric
cliques whose WL order is opposite to alphabetical and whose round-1 order also differs (so round 2
is exercised) — plus all regressions (`tests/selfhost_frontend.rs` 19/19). Remaining axis-2 surface:
call arguments / params-in-multi-function; and a recursion clique of 3+ members (the WL refinement
generalises, but the ordinal-resolution `aord` shortcut is two-member-specific).

Landed 2026-06-15: CALL ARGUMENTS + function PARAMETERS in a multi-function program (the
create_function / two-function-DAG path). Until now every multi-function callee was no-parameter and
every cross-symbol/recursive call no-argument; this adds both. (1) Parameters on a function in a
two-function program: the callee's typed body resolves `param_ref`s, its FunctionSignature lists the
parameter type hashes, the ProgramRoot's `param_names` carries the names (still symbol-hash-ordered,
so they diverge from the display-name-ordered `names` array — exercised when the dependency toposort
contradicts alphabetical), and the create_function migration's operation/postcondition `params`
array lists `{name, type}` pairs (name = source slice, type = the raw type NAME — the dual
serialization). (2) Call arguments: the typed `call` node's `args` array holds the argument
expressions' content hashes, parsed in the CALLER's scope (so an argument may reference the caller's
own parameters). The parameter scope is RE-DERIVED from source where needed (`build_fn_objs` and the
new `build_op_params` find the `(` past the name and run `scan_params`), rather than widening the
already-large `Sl` carrier with a 120-byte `Scope`. Argument lists are parsed by a LOOP
(`parse_args`, the proven `parse_add`/`raw_add` shape — fresh-record accumulator, fresh-record
return) because a self-recursive parser that ALSO calls the clique-member `parse_bitor` through an
aggregate return miscompiles to a v0 SIGTRAP at depth >= 2. The param-name fragments are threaded
into the two-symbol ProgramRoot's hash-ordered `param_names` via a small `order_pp` helper (paralleling
`order_triples`, so each fragment stays paired with its symbol across the display->hash re-sort). Scope:
i64/bool parameters (a sized-parameter argument would need the callee's parameter type pushed down as
the argument's expected type — deferred, like the documented axis-1 unification gaps); `raw_call`
arguments are NOT yet emitted (in any two-function program the canonical-first function — the only one
whose body the raw serializer touches — never contains a call, so they are not root-reachable; needed
once a canonical-first function can call, i.e. 3+ functions). Every form was spiked against
`codedb history --json` / `emit-objects` and the whole increment reproduced byte-exactly in Python
before any `.cdb`; all 147 functions stayed under the v0 frame budget (validated offline via
`emit-ir` + the backend-formula replica). One v0 buffer-sizing bug surfaced and was fixed: the
op-params builder sized its buffer from the SOURCE span, but the `{"name":...,"type":...}` envelope
makes the output ~4x larger, so a 3-parameter callee overflowed `string_with_capacity` (a SIGTRAP) —
now sized from the parameter count. Root-hash equality holds across 24 fixtures — parameters on
independent functions (both source orders, both having parameters, bool/sized parameters), call
arguments (single / two / three / four / nested / zero), the call in arithmetic / `if` / `let`, an
argument using the caller's parameter, longer parameter names, and the toposort != alphabetical
divergence with a parameter-bearing callee — plus all regressions (`tests/selfhost_frontend.rs` 20/20).
Remaining axis-2 surface: parameters + arguments in RECURSION GROUPS (self- and mutual-recursion —
`build_rg_member` / `build_rg_root` still parse member bodies with an empty scope and emit empty
param_names); and a recursion clique of 3+ members.

Landed 2026-06-15: PARAMETERS + ARGUMENTS in RECURSION GROUPS (self- and mutual-recursion),
completing the call-arguments/params surface for two-function programs. A recursion-group member may
now have parameters, and its self/cross-member call may pass arguments. Self-recursion (a one-member
group): `build_rg_root` re-derives the member's parameter scope (the shared `derive_params` helper),
re-parses the body with it (so param_refs and the argument-bearing recursive call resolve), builds the
signature with the parameter types, and emits the parameter names into the ProgramRoot's param_names.
Mutual recursion (a two-member group): `build_rg_member` does the same per member, and the parameter
fragments thread into the hash-ordered param_names tail (a `PP` carrier, mirroring the two-function
root). The genuinely new mechanic is the WL member-ordering colour: its static signature now carries
the member's parameter TYPE NAMES (`src/lib.rs` `recursion_member_static_sigs`:
`{"effects":[],"params":[<type names>],"regions":[],"return":"<type>"}`), so `member_color` /
`build_rec_sig` emit them (via `push_ptypes_at`) — the recursion order can otherwise diverge from the
Rust importer. Two corrections to the increment-A understanding surfaced (both reproduced byte-exactly
in Python before fixing): (1) **`raw_call` arguments ARE required here** — a recursion-group root needs
no migration, but each member's body is raw-serialized for its WL ordering colour (`member_color` →
`raw_bitor` → `raw_call`), so a call with arguments must serialize them or the colour (and hence the
member order and the symbols) diverges; the increment-A note that `raw_call` args were unreachable
held only for the two-function DAG migration path, not the recursion-colour path. The fix is the raw
counterpart of `parse_args` (`raw_args`, a loop). (2) The recolour's peer-call marker changed from the
empty-`"args":[]` form (which assumed no-argument calls) to the argument-independent `"kind":"call"`
(the arguments, emitted before it, are recoloured by the same logic if they nest a peer call). The
member-order bug was localized by reproducing the importer's byte-level recolour in Python with
argument-bearing vs argument-less bodies — only the latter mis-ordered the one asymmetric clique
(`a(x)=b(x)+1`, `b(x)=a(x)`) whose order depends on the erased argument structure (the `if`-shaped
cliques are ordered by their then-branch literal regardless). One v0 reality recurred: bundling the
two display-name ranges into a record (`NR`) kept the two-member root assembler under the 8-machine-
parameter cap. Root-hash equality holds across 9 recursion-group fixtures — self-recursion (one/two
parameters, i64/bool results) and mutual recursion (both source orders, asymmetric cliques whose WL
order is opposite alphabetical, i64 and bool members) — plus all regressions
(`tests/selfhost_frontend.rs` 21/21). Remaining axis-2 surface: a recursion clique of 3+ members (the
WL refinement generalises, but the `aord` two-member ordinal shortcut and the ord0-symbol-hash-sorts-
first assumption are two-member-specific).

In progress 2026-06-15: 3+-member recursion cliques, taken all the way to the Rust importer's
`clique_label_search` (the backtracking individualization-refinement graph canonicalisation that
`canonical_clique_order` falls back to when 1-WL colour refinement does not discretise the clique —
`src/lib.rs:1713`/`:1787`). Decomposed into a four-increment ladder: (1) the n-function
`create_function` chain (independent + DAG — also a correctness fix, since the importer currently parses
exactly two top-level functions and mis-parses a third; and the k-way root-array sort it builds is shared
with the clique root); (2) a three-member clique whose 1-WL refinement discretises, plus the SCC
detection that tells a clique from increment 1's DAG; (3) four-or-more-member discretising cliques (the
real symbol-hash sort — for four or more members the recursion-group symbols are no longer in ordinal
order); (4) `clique_label_search` itself, for the non-discretising and fully-symmetric cliques. The
n-function chain mechanics were reproduced byte-exactly in Python for n = 2…5 (independent, chain,
diamond, fan-in) before any `.cdb`: the canonical order is a Kahn topological sort (callee first) with an
alphabetical tie-break, each symbol's birth history is the previous migration's running history (genesis
for the first), the importer computes n−1 migrations and builds the final n-symbol root directly, and a
migration's `parent_history_hash` is the previous history hash (verified non-null for the second
migration onward — the null hypothesis fails the migration hash). A three-function DAG therefore finally
exercises `raw_call` (the canonical-first-but-one function now carries a migration whose raw body
contains a call). Landed as the first step (a transparent refactor, suite 21/21 + the local smoke 51/51
unchanged): `build_mig1` is generalised with a parent-history argument (the empty string for the genesis
migration → JSON `null`) and a canonical ordinal (rendered into the birth seed), via small `MigCtx` /
`MigPar` carriers; the additions pushed `mig1_pay_a` over the v0 frame budget (4384 > 4095, caught
offline by the frame sweep), so it was split into `mig1_pay_a` / `mig1_pay_a2` (a `PayMid` carrier),
bringing the max frame back to 3952. Empty-parent + ordinal 0 reproduces the old payload exactly, so
every two-function root is byte-identical. Next: the general-m `assemble_root` (display + hash selection
sorts with Copy accumulators; byte-copy emission that reads the owned source/hash buffers via
`string_get`) plus the unrolled three-function chain and the n-function top-level parse.

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

Goal: the self-hosted compiler can read a source path from its command line.

Status: implemented. Resolves R12 — as ambient-input BUILTINS rather than an
entry-signature change: `arg_count() -> i64`, `arg_len(i) -> i64`,
`arg_byte(i, j) -> u8` (process arguments, program name excluded; `io`
effect; out-of-range = eval error / native trap). The entry stays
parameterless, so every existing harness shape is preserved. Natively the cc
link harness captures argc/argv and the lowered ops call its runtime
accessors (`codedb_arg_*`, the malloc/free platform-symbol pattern); the
reference evaluator/tracer read the same list seeded by `--process-arg` on
`eval`/`trace`/`debug` — eval == native on the same arguments
(tests/argv_native.rs). `std.io.arg_string(i)` composes the byte reads into
an owned string with a move-only loop accumulator (loop-carried drop glue) —
the source-path read the self-hosted front-end needs. envp stays deferred
until something forces it.

Deliverables (delivered):

```text
arg_count/arg_len/arg_byte builtins: typed nodes, eval/trace parity, lowered
  ops, both native backends via the cc-harness argv runtime
capability surfacing: the `args` capability + codedb_arg_* platform externals
  in the build plan; entry metadata args.supported = true
std.io.arg_string; --process-arg on eval/trace/debug
(deferred: envp; argv in an entry signature — the builtin form unblocks rung A)
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

The checked-view gate rests on `import -> export -> import` being a root-hash
fixpoint (SPEC_V3 §11). That holds because the importer processes parsed items in a
deterministic, **source-order-independent canonical order** (all type definitions
first, then functions/externals; each Kahn-toposorted by dependencies with an
alphabetical tie-break, mutually-recursive cliques as single units), so a program's
migration sequence — and therefore every deterministic birth identity (§10) and the
root hash — is a function of the item SET, not of how the source happens to be
ordered. Without it, a hand-written source in any order other than the projection's
canonical order would re-import (from the name-sorted projection) with a different
migration history and a different root, even though the projection text is
byte-stable. The source is always already a valid topological order (the importer
fails to resolve a forward reference otherwise), so canonicalizing it never violates
a dependency. `tests/import_order.rs` pins order-independence (two source orderings of
one program reach the same root) and the non-canonical-source round-trip fixpoint.

## Suggested milestone cuts

### Milestone V3.0 — Foundations

Includes: Phases 1–3 (docs, architecture paydown, agent spine). Status: complete.

```text
Success: features add with a small edit surface, and concurrent agents build them
  through structural edits with proof-carrying receipts and semantic merge.
```

### Milestone V3.1 — Sound Recursive Frontier

Includes: Phases 4–7 (drop glue, recursion groups, recursion, pattern matching).
Status: implemented. Recursion (self and mutual) compiles native and matches the
oracle; conditional and field-granular drop glue is sound (double-free-on-run
verified; no allocation interposer yet — see the follow-on note) — including the two
dimensions COMBINED (a record field moved in only some branches while a sibling field
stays live, which now lowers and is double-free-verified); verify accepts recursive
call graphs and recursion-group objects; recursion-group content identity is canonical
— member ordinals derive from clique structure via individualization-refinement (which
computes an order-invariant canonical FORM), with automorphism-orbit ties broken by a
stable per-member key (the member's module-qualified name) because a name-independent
*distinct* ordinal assignment is impossible for structurally-indistinguishable members;
so two source orderings of one clique — including a vertex-transitive clique of
byte-identical-bodied members (a true automorphism that 1-WL cannot discretize and an
argument-slot trick cannot separate) — produce the same hash, both pass verify, and
import→export→import is a fixpoint; scalar literal `case`
with `_` and exhaustiveness compiles native, projects round-trip (including a nested
`case` in a non-last arm), and steps under `trace`/`debug`. Case-traversal of a
recursive `box<Node>` heap also compiles native: an `unbox` (deref-by-move) builtin
and move-only `box` case-arm binding free each node exactly once (`0 leaks`,
double-free verified). Mutually-recursive *type* definitions (D1) are also supported
(a `CreateTypeGroup` clique, mirroring `CreateRecursionGroup`; canonical member
ordinals; box-broken size cycle), so per-node-data recursive structures — a
`Cons`↔`List` cons-list `sum` and an `Expr`↔`Pair` tree-walking evaluator — compile
native and round-trip. A field reached through a `box` deref now fails closed with a
clean `unsupported_move` diagnostic (was an opaque lowering crash).

Documented follow-on R14/structure surface: `if` guard and nested-enum-destructuring
patterns (range patterns `lo..hi`/`lo..=hi` are now implemented). Inline (non-`box`)
move-only enum-payload moves out of a `case` arm are now implemented too — a consumed
(param/local) move-only enum scrutinee's inline aggregate payload is read out by a
`Load`-aliased pointer + `Store` memcpy (a shallow byte move; the consumed enum is
never dropped, so each owned resource transfers exactly once), pinned at runtime scale
by `tests/leak_interposer.rs`. Moving a payload out of a *temporary* (non-place) enum
stays fail-closed (a temporary is not drop-tracked). `verify` recomputes each recursion/type
clique's canonical ordinals from the re-projected source and rejects a permutation —
covered now on BOTH sides: the positive path (valid automorphic cliques must not
false-reject) plus a negative regression that mints a clique with non-canonical member
ordinals through the create path and asserts `verify` rejects it
(`recursion_group_ordinal_verify_tests`). The lowered-IR verifier proves drops occur at
most once (no double-free); the no-leak half of exactly-once rests on lowering's static
drop placement and is now independently confirmed at runtime by an allocation interposer
(`tests/leak_interposer.rs`): it counts malloc/free in the built native binary and asserts
the net (alloc - free) is invariant to a program's allocation count, so a skipped-drop
leak — invisible to the double-free guard — makes the net grow and is caught (verified
discriminating against a simulated skipped-drop). Former oracle caveat, now bounded: the
reference evaluator recurses on the host stack, so a deeply/infinitely recursive program
would overflow it; a call-recursion ceiling (`MAX_EVAL_CALL_DEPTH`) now converts that
process abort into a clean error. It is an oracle robustness bound, not a language limit —
the native backend runs on the OS stack and compiles + runs the same program (pinned by
`deep_recursion_evaluator_ceiling_is_an_oracle_bound_not_a_native_limit`).

```text
Success: recursion compiles native; drops occur exactly once across conditional
  and recursive paths; verify handles recursive call graphs.
```

### Milestone V3.2 — Self-Hosted Oracle

Includes: Phase 8 (reference evaluator in CodeDB, rung 0). Status: COMPLETE
(see Phase 8 — the .cdb evaluator runs natively, result-equal to the Rust
evaluator across the conformance sweep, the per-feature fixtures, and the
example corpus incl. the full sha256 digest; §11 checked-view gate green).

```text
Success (met): CodeDB-eval == Rust-eval on the corpus — a three-way oracle.
```

### Milestone V3.3 — Expressiveness for a Front-End

Includes: Phases 9–14 (ints/bitwise/casts/modulo, early exit, loops, strings/fmt,
array fill, generics). Generics (Phase 14) is delivered for records, enums, AND
functions — parametric `record`/`enum`/`fn` monomorphized natively at two-plus
instantiations (a generic function's instances materialized as derived symbols at
the lowering seam), now including recursive and mutually-recursive generic functions
through a generic recursion group (the clique binds its members' `<T>` signatures
before any body is typed; the instances co-materialize at the lowering seam and
recurse by-symbol). The remaining follow-on is a recursive generic function over a
*mutually-recursive generic type* clique (a generic `CreateTypeGroup`, D1), which
fails closed (its `<T>` is not yet threaded through `CreateTypeGroup`). The
expressiveness acceptance is met: `tokenizer.cdb` and `sha256.cdb` both compile
native (sha256 rolled into loops over a Copy `array<u32, 64>` updated by `array_set`,
eval == native on all eight digest words — see Phase 11's array-update note).

```text
Success (met): sha256.cdb and tokenizer.cdb compile native; the language can express a
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
