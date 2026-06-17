# SPEC_V3.md — CodeDB Self-Hosting Semantic Programming Model

Status: Draft 1.0
Scope: v3 design track built on the v2 native semantic programming model

## 1. Thesis

CodeDB v3 turns the v2 native compiler into a self-hosting one. The compilation
pipeline itself becomes content-addressed CodeDB objects, built concurrently by
agents through structural operations.

A program is still an immutable, content-addressed semantic DAG plus replayable
migration history. V3's new claim is that *the compiler is such a program*. The
typed-DAG-to-native pipeline — parsing, type checking, borrow/effect/move/drop
checking, layout, lowering, native object emission, and linking — is expressed
as CodeDB objects and reproduces the trusted Rust compiler's artifacts exactly.

The central v3 rule is:

```text
A pipeline stage is self-hosted only when the CodeDB-hosted stage reproduces the
Rust stage's artifact, hash-for-hash or byte-for-byte, and passes verify and
replay. A stage that runs only in the reference evaluator is not self-hosted.
```

V3's central question is:

```text
Can CodeDB express its own compiler — and can agents build that compiler
concurrently through structural edits with proof-carrying change-impact?
```

These are one question, not two. Self-hosting is the forcing function that makes
the language real (you cannot fake a compiler with evaluator support), and the
self-hosted compiler is built *through* the structural-edit layer, which makes
the agent-native story real on the most demanding workload available: CodeDB's
own compiler. The "language-real" and "agent-native" goals converge here.

## 2. Relationship to v2

V2 made CodeDB compile useful native programs without interpreter fallback. V3
keeps every v2 capability and adds three things:

```text
1. the expressiveness the compiler-as-a-program forces (recursion, generics,
   strings, pattern matching, sized integers, bitwise, casts, loops, early exit)
2. a sound representation of cyclic constructs in an acyclic content-addressed
   DAG (recursion, mutual recursion, function values, self-reference, packages)
3. a minimum agent-editing spine — semantic merge, optimistic concurrency, and
   proof-carrying receipts — sized to building the compiler concurrently
```

V3 must not weaken any v2 invariant:

- immutable content-addressed objects and deterministic hashing;
- stable semantic identity (rename is metadata-only; the interface/implementation
  hash split firewalls callers from callee body changes);
- the native completion rule (no interpreter or host-call fallback);
- fail-closed verification;
- replayable histories and replay/export/import round-trips.

One principle is restated because it is easy to violate: the source text and the
`emit-c` output are **projections** — human-readable views of the semantic
objects. They are not the compilation path, and self-hosting may not route
through them. The only compilation path remains:

```text
typed semantic DAG -> lowered IR -> native object -> link plan -> executable
```

## 3. Design principles

1. The compilation pipeline is the self-hosting target. Projections are not.
2. Projections (source text, `emit-c`) are views, never a compilation path. A
   self-hosted stage may not produce or consume them as authoritative artifacts.
3. Every self-hosted stage is validated against a determinism oracle: its output
   is hash-identical or byte-identical to the Rust stage's output.
4. The Rust compiler remains trusted stage-0 and the oracle. Self-hosting does
   not delete it; it reproduces it.
5. Self-hosting is staged at the lowered-IR seam. A mixed compiler — CodeDB
   front-end feeding the Rust native backend at the IR boundary — is a
   legitimate interim, because lowered IR is a real intermediate, not a
   projection.
6. Native object emission is in scope and staged last. It may not be scoped away
   by lowering to C.
7. Every new language feature must preserve identity and provenance invariants.
   "What is the stable identity of X, and how does blame attach to it?" is a
   required design answer for every new construct.
8. Cyclic constructs get a well-defined content hash through a recursion-group /
   by-name fixpoint object, never an implicit hash cycle.
9. Soundness gates expressiveness. Conditional and field-granular drop glue must
   be solved before early exit, loops, and recursion are added.
10. The agent-editing spine is load-bearing v3 infrastructure, not a deferred
    product. The self-hosted compiler is built through structural operations with
    proof-carrying receipts and semantic merge.
11. The v2 native completion rule still holds for every new language feature: no
    interpreter fallback, no opaque host-call "support," no completion without
    native-required tests.
12. Verification remains mandatory and extends to every new object kind
    (recursion groups, generic instances, function values).
13. The committed `.cdb` projection is a checked view, never the source of truth.
    The authoritative program is the content-addressed object DAG, stored in git
    as a deterministic export and reproducible on rebuild because identities are
    deterministic (see §10 and §11).
14. The Rung-A front-end spine is load-bearing infrastructure. The self-hosted
    importer/front-end may not grow by hand-transcribing every Rust importer case
    into one monolithic `.cdb`. Rung A requires a reusable internal
    compiler-construction spine — source bytes → token stream → parsed item
    table/AST → item dependency graph and canonical grouping → typed semantic
    object constructors → canonical object writer → root/migration/history hash
    builder (see §5A). The spine is implementation infrastructure; it does not make
    source text authoritative — source stays a checked projection, the
    authoritative result is still the content-addressed object DAG and its
    migration history.
15. Canonical serialization is a stage, not string glue. Every canonical payload
    used by Rung A is emitted through a shared canonical writer + object-builder
    library that owns canonical key layout, JSON escaping, payload size
    measurement, emission, object-domain framing, schema versioning, and object
    hashing. Once that library exists, stage code may not hand-assemble ad hoc JSON
    for a new object kind; a new object kind is accepted only with an
    object-builder path, a Rust-oracle fixture, and a malformed-object verification
    test.
16. Raw migration serialization and typed object serialization share one AST. Rung
    A has two real serializations — raw operation serialization (→ migration_hash /
    history_hash) and typed object serialization (→ Expression / FunctionDef / root
    hash). Both must derive from one parsed item/AST representation. Duplicate
    raw-vs-typed parser logic may exist temporarily during migration, but a
    sub-stage is not complete until that duplication is retired or mechanically
    checked for equivalence.
17. Unsupported importer shapes fail closed. The importer may be intentionally
    incomplete during Phase 15, but it must never route an unsupported program
    through an unrelated fallback that can produce a plausible-but-wrong root. Every
    unsupported grammar or item-graph shape must instead emit a deterministic
    unsupported diagnostic, trap deterministically under a native-required fixture,
    or return a sentinel the harness treats as expected-unsupported. A wrong root is
    never an acceptable fallback.
18. Performance artifacts are part of self-hosting evidence. Rung A and later rungs
    must preserve deterministic artifact caching (lowered IR, type layout,
    dependency sets, interface/implementation hashes, native objects, link plans,
    executables). A self-hosted stage is checked not only for semantic equality but
    for enough artifact identity to avoid rebuilding unchanged work; the mixed
    compiler must not pay avoidable repeated work such as lowering the same function
    once during planning and again during object emission.
19. Backend health is a self-hosting prerequisite. When backend limitations force
    systematic source-level contortions in the self-hosted compiler, they become
    self-hosting blockers — large aggregate stack-frame addressing, aggregate-return
    / hidden-return-slot correctness, loop-accumulator return correctness, move-only
    borrow-before-move correctness, and stack-slot reuse/liveness for large compiler
    functions are correctness/progress gates, not incidental polish. Conservative
    codegen is fine; depending on undocumented source-shaping idioms to dodge native
    SIGTRAPs is not.

## 4. Two completion rules

V3 has two layered definitions of "done."

The **native completion rule** (inherited from v2 §4) applies to every new
*language feature* v3 adds. A feature — recursion, generics, a new integer
width, a bitwise operator — is complete only when all relevant layers support
it: payloads, canonical hashing, edges, projection syntax, structural apply,
patch support where exposed, type checking, region/borrow/move/drop checking,
effect checking, reference evaluation as oracle, semantic tests, native-required
tests, trace/debug locations, lowered IR, native object backend, ABI/layout,
link path, artifact cache keys, replay/export/import, and verification.

The **self-hosting completion rule** (new in v3) applies to every *pipeline
stage*. A stage is self-hosted only when:

```text
the stage is expressed as CodeDB semantic objects
the stage compiles natively (it obeys the native completion rule)
the stage's output matches the Rust stage's artifact under the determinism oracle
verify validates the self-hosted stage's objects
replay/export/import round-trips the self-hosted stage
```

A stage that produces correct results only in the reference evaluator is not
self-hosted. A feature that the self-hosted compiler needs but that is not itself
native-complete is not done.

## 5. The self-hosting ladder

Self-hosting is staged so each rung produces a deterministic, hash-comparable
artifact, giving a built-in oracle at every seam. The reference evaluator is the
oracle and a Pillar-1 warm-up; it is off the compilation path, not a rung of it.

```text
Rung 0 (warm-up, off-path) — reference evaluator in CodeDB
  the smallest recursive IR-walker; forces recursion (R1) and pattern richness (R14)
  oracle: result equality vs. the Rust evaluator on the existing test corpus
  yields a three-way oracle: CodeDB-eval == Rust-eval == native backend

Rung A — front-end to lowered IR
  parser/importer -> type check -> borrow/effect/move/drop -> layout -> lowering
  output: the lowered-IR artifact
  oracle: IR-hash equality vs. the Rust front-end
  interim: the mixed compiler — CodeDB front-end + Rust native backend meet here
  forces: strings (R15), content hashing / SHA-256 (R4, R5, R6), recursion (R1),
          pattern matching (R14), generics for compiler data structures (R11)

Rung B — native object emission
  lowered IR -> native object (.o)
  oracle: byte-identical .o vs. the Rust emitter (artifacts are deterministic)
  forces: bitwise ops, sized/unsigned integers, byte-buffer building (R4, R5, R6)
          intensely — machine-code encoding is bit-level

Rung C — link plan
  object set -> link plan -> executable
  oracle: identical link-plan JSON vs. the Rust linker driver
```

Each rung is a native-required acceptance fixture under the v2 discipline. The
importer computes content hashes, so Rung A cannot be self-hosted until the
language can compute SHA-256 — the content-addressing core literally cannot host
itself until R4/R5/R6 land. Native emission (Rung B) is the largest and least
thesis-novel rung; it is staged last but it is not optional, because in CodeDB's
sense of "compiler" the native path is the compilation path.

## 5A. Rung A internal compiler spine

Rung A is the front end to lowered IR. Its output oracle remains lowered-IR hash
equality, and its earlier importer sub-stage uses object/root-hash equality.
Internally, Rung A is divided into a spine of reusable compiler libraries — it is
not a monolithic transcription of the Rust importer.

```text
compiler/front/io.cdb          buffered source ingestion
compiler/front/json.cdb        canonical JSON measurement + emission + escaping
compiler/front/object.cdb      typed CodeDB object builders + object hashing
compiler/front/ast.cdb         parsed item table and AST arena
compiler/front/graph.cdb       dependency graph, Kahn order, SCCs, recursion ordering
compiler/front/migration.cdb   birth seeds, migration JSON, history hashing
compiler/front/import.cdb      orchestration: source -> root hash / object DAG
```

The exact file split may evolve, but the responsibilities must remain separated.
`import.cdb` orchestrates these libraries; it is not a monolithic copy of the Rust
importer.

### 5A.1 Parsed item table and AST

The importer first constructs an item table. Each top-level definition records:

```text
kind: function | extern | record | enum | generic | recursion_group_member | type_group_member
module
name span and canonical display name
signature/type header information
parameter/member spans and type names
body AST root, when present
source span bounds
```

Expression and type syntax are represented as compact AST nodes or arena records.
The representation may be transient compiler data rather than committed CodeDB
program objects, but it must have a deterministic debug/probe projection for oracle
localization. The AST is the shared input to (1) raw operation serialization for
migration/history hashes and (2) typed semantic-object construction for CodeDB
object hashes.

### 5A.2 Item graph and canonical ordering

The importer computes a dependency graph over the item table before creating
symbols. Canonical import order is source-order independent:

```text
DAG items: Kahn topological sort, callee/dependency before dependent, alphabetical tie-break
recursive function SCCs: one RecursionGroup per strongly-connected component
recursive type SCCs: one TypeRecursionGroup per strongly-connected component
root arrays: sorted by the root normalizer's own per-array keys
```

The graph library, not ad hoc importer branches, owns these rules. It must support
mixed programs — type definitions, functions, externals, recursion groups, and
eventually generic instances in one root — and fail closed on graph shapes it
cannot yet lower.

### 5A.3 Canonical object writer and object builders

The canonical writer exposes a two-pass discipline:

```text
measure(payload) -> exact or conservative capacity
emit(payload, buffer) -> canonical bytes
hash(kind, schema, payload) -> content hash
```

All object builders use this writer. Builders include at least: Type, SymbolBirth,
FunctionSignature, Expression, FunctionDef, ExternalFunctionDef, RecordDef, EnumDef,
TypeDef, RecursionGroup, TypeRecursionGroup, ProgramRoot, Migration, History.
Canonical key order must be explicit in builder code and tested per object kind. No
builder may rely on a host-side `sort_keys` mental model unless the emitted bytes
are proven identical to the Rust canonical-JSON oracle.

### 5A.4 Dual emitters from one AST

For every expression form accepted by the importer, the AST library provides
`emit_raw_operation_ast(node)` (migration operation / postcondition body) and
`build_typed_expression(node)` (Expression objects and type hashes). The test gate
for a new expression form must include a fixture where the raw AST contributes to a
migration hash AND a fixture where the typed expression contributes to a ProgramRoot
hash.

### 5A.5 Unsupported shapes

Unsupported input must be classified before object construction (mixed SCC+DAG
shapes not yet supported; programs above current fixed item-count limits; forward
references not representable yet; named-type forms not yet implemented; ill-typed
programs when the sub-stage only claims valid-program root equality). These cases
fail closed (§ design principle 17); they may not fall through to a smaller importer
path.

### 5A.6 Diagnostics and localization artifacts

Each Rung-A sub-stage exposes deterministic probes — token, AST/item-table,
item-graph/order, object dump / per-object hash, migration/history, typed-object,
layout JSON, lowered-IR hash. The probes are development artifacts for localizing
oracle divergence, not compilation paths.

### 5A.7 Rung A acceptance rule

Rung A is accepted only when:

```text
1. the front-end stage is expressed as CodeDB semantic objects and compiles natively;
2. source import produces object/root hashes identical to the Rust importer on the acceptance corpus;
3. type checking, borrow/effect/move/drop checking, layout, and lowering produce artifacts identical to Rust at their seams;
4. raw migration serialization and typed object construction derive from one parsed AST/item representation;
5. canonical payloads are emitted through the shared object-builder/canonical-writer layer;
6. unsupported source or graph shapes fail closed;
7. replay/export/import round-trips the self-hosted stage and its checked projection;
8. artifact caching avoids known repeated work, especially duplicate lowering between planning and object emission;
9. backend-health gates pass on all supported native targets.
```

## 6. Cyclic content-addressing (the keystone)

A content-addressed DAG is acyclic. A recursive function embeds a reference to
itself, which would be a cycle in the object DAG. V2 sidesteps this: recursive
*types* go through `box<T>` indirection, and recursive *functions do not exist*
([ROADMAP.md](ROADMAP.md) R1). V3 must solve it, because the compiler is deeply
recursive and mutually recursive.

A single design — a **recursion-group / fixpoint-reference object** — gives a
mutually-recursive clique a well-defined, replayable content hash:

```text
edges within a recursion group are by-name (fixpoint) references, not content edges
edges into a recursion group are ordinary content edges
the group's content hash is defined over its members' bodies with internal
  references canonicalized to stable in-group identities, so the clique hashes
  deterministically and replays
```

This one decision unlocks four roadmap items at once:

```text
R1  recursion and mutual recursion
R13 first-class function values (a function value and recursion share the
      fixpoint machinery)
D1  self-referential movable records (same pin / by-name discipline)
D7  package registry (the cross-module dependency graph is the cycle problem at
      module scale)
```

The required identity answer (principle 7): what is the stable identity of a
recursive clique, a function value, and (with generics) a monomorphized
instance? Verify must handle recursive call graphs for effects, borrows, moves,
and drop ordering — the call graph is no longer a tree.

## 7. The drop-glue prerequisite

V2's "drops occur exactly once" guarantee currently holds only because partial
and conditional moves are rejected fail-closed. Conditional drop glue (an owned
value moved in only some `if`/`case` branches) and field-granular drop glue (a
partial move out of a record field or array element) remain deferred.

Early exit (R7), unbounded loops (R8), and recursion (R1) all stress exactly
this dataflow. V3 must solve drop placement for partial and conditional moves
**before** those features are added, or each will either widen the fail-closed
rejection surface until it is unusable or convert a fail-closed gap into an
unsoundness. This is the highest-priority soundness item in v3 and a hard
predecessor of Rung 0/A recursion work.

## 8. Expressiveness floor

V3 adds only the expressiveness that self-hosting forces, mapped to the rung
that forces it. The R-item IDs are defined in [ROADMAP.md](ROADMAP.md).

```text
R1  recursion / mutual recursion        Rung 0/A   structural; needs Pillar 1 + §7
R14 pattern richness                    Rung 0/A   forced by IR/AST dispatch
R15 string operations (as stdlib)       Rung A     forced by text processing
R3  int<->string (as stdlib)            Rung A     forced by diagnostics
R4  bitwise operators                   Rung A/B   forced by SHA-256, then codegen
R5  sized / unsigned integers           Rung A/B   forced by SHA-256, then codegen
R6  numeric casts                       Rung A/B   forced by SHA-256, then codegen
R7  early exit / error control flow     Rung A     forced by malformed-input paths
R8  unbounded loop                      Rung A     forced by worklist/fixpoint passes
R2  remainder / modulo                  Rung A     small, pure plumbing
R9  array fill / repeat initializer     Rung A     small, pure plumbing
R11 generics / parametric types         Rung A     big rock; its own design pass
R12 process arguments (argv)            Rung C     forced by a real compiler CLI
```

Categorization:

```text
pure pipeline plumbing (cheap, identity-neutral): R2, R4, R5, R6, R9, R14
structural (interact with §7 drop glue and Pillar 1): R1, R7, R8
big rocks staged on their own: R11 generics
```

R11 (generics) is the one large rock the compiler genuinely needs — its data
structures are full of `Vec<T>`, `Option<T>`, and `Result<T, E>`. It is designed
as its own pass, not rushed, because monomorphization is where the god-module
risk and the identity question (§10) bite hardest.

Notably out of the floor: R10 (floating point) is **not** required to self-host
and is deferred (§12). Self-hosting forces almost the entire R-list *except*
floats — which is the strongest evidence that self-hosting is the right forcing
function: it demands honest, broad completeness rather than a cherry-picked demo.

## 9. The agent-native spine (minimum viable)

V3 keeps the agent-editing layer at the minimum that lets several agents build
the compiler-in-CodeDB concurrently. The full agent-native platform is v4
([SPEC_V4.md](SPEC_V4.md)). Required for v3:

```text
semantic merge
  common-ancestor root + migration replay + semantic conflict detection +
  hash-pruned tree diff + build-impact recomputation
  (the SPEC names this; v0/v2 have no merge algorithm — v3 needs the minimum one)

optimistic concurrency
  the existing --expect-root protocol (applied / already_applied / conflict)
  binds each structural write to the root the agent inspected

proof-carrying receipts
  every structural change returns, before commit: typecheck summary, borrow /
  effect / capability delta, build-impact verdict (metadata_only / relink_only /
  recompile_dependents / ...), and a semantic diff
```

Sizing: enough for a handful of agents editing the compiler concurrently without
falling back to editing `.cdb` projection text. Falling back to text would
violate "text is not the source of truth" and silently kill the concurrency
goal. The agent spine is therefore v3 infrastructure, not a v4 preview.

## 10. Identity and provenance invariants

Every new construct must define its stable identity and how provenance attaches,
preserving the v2 guarantees:

```text
rename remains metadata-only
the interface_hash / implementation_hash split still firewalls callers from
  callee body changes
a body change does not re-identify a symbol
```

New required answers:

```text
recursion group  — the clique's stable identity and per-member identity (§6)
function value    — the identity of a function used as a value
generic instance  — a monomorphized instance's derived identity, and how
                    blame/why traces instance -> generic definition
```

Verify must recompute these identities deterministically and reject malformed or
inconsistent ones.

Identities are deterministic, resolving the SPEC §29 open question on birth
seeds. A symbol's birth identity is derived from the creating migration and its
ordinal within that migration — never from its display name, and fixed at birth:

```text
birth identity = f(creating_migration_hash, in_migration_ordinal)
  - deterministic: no random nonce; the same history reproduces the same identities
  - name-independent: a later rename is metadata and does not re-identify
  - immutable: only the creating migration sets the identity
```

Because identity is a pure function of history, a program's identities and its
root hash reproduce exactly on rebuild. This is what lets the committed source be
a checked view (§11) rather than the authoritative form.

The migration/history builder owns all birth-seed strings and deterministic
birth-history selection. Individual importer cases may *request* a birth for a
symbol, type, field, variant, recursion-group member, or generic instance, but may
not hand-assemble the final seed string outside the builder. This keeps provenance,
blame, and root reproducibility tied to one implementation of the identity rules.
For recursion groups and type-recursion groups, the graph/order library owns member
ordinals — the symbol builder receives the chosen ordinal; it does not recompute
canonical clique order locally.

## 11. Source representation and durable storage

A CodeDB program — including the self-hosted compiler — is authoritatively the
content-addressed object DAG, not any text file. This section fixes how that DAG
lives in a git repository.

```text
authoritative form: a deterministic, canonical export of the object DAG —
  migration history (NDJSON) and/or a current-root object snapshot, plus a
  committed root_hash pin
checked view: the .cdb text projection, regenerated by `export` and validated
  against the objects — never the source of truth
disposable cache: the SQLite database — a materialization of objects and indexes,
  never committed (it is `/target`, not source)
```

The committed `.cdb` under `compiler/` (and elsewhere) is a checked view. The
gate that keeps the view honest:

```text
import the committed source -> verify -> re-export
  the re-exported projection is byte-stable
  the rebuilt root_hash matches the committed pin
deterministic birth seeds (§10) make this rebuild reproducible
```

The checked-view gate depends on canonical importer order over the parsed item set.
Importing a source projection, exporting it, and importing it again must reproduce
the same root because the importer canonicalizes the semantic item graph (§5A.2),
not because the source text happened to appear in the projection's order. This
applies to mixed roots containing types, functions, externals, recursion groups,
and eventually generic instances. The parsed AST/item table is not an alternative
source of truth; it is an internal compiler artifact used to reproduce the
authoritative object DAG and migration history.

Review and history:

```text
review uses the .cdb projection diff and CodeDB's own semantic diff (classified
  changes with build-impact), which is richer than a textual diff
git commits are the squash boundary for the underlying migrations: shared history
  is curated per commit, not one entry per agent keystroke
```

Forbidden:

```text
treating the committed .cdb text as authoritative source
committing the SQLite database as source
```

Trajectory: in v3, git carries the checked-view projections plus the canonical
export. As semantic merge and proof-carrying receipts mature
([SPEC_V4.md](SPEC_V4.md)), CodeDB's migration history becomes the primary
version control and git recedes to hosting the canonical export.

## 12. Acceptance programs

V3 is accepted through self-hosted pipeline stages and the forcing-function
programs that drive the floor, each native-compiled with an oracle.

```text
reference evaluator in CodeDB        Rung 0   oracle: result equality vs Rust eval
self-hosted front-end to lowered IR  Rung A   oracle: IR-hash equality
self-hosted native object emission   Rung B   oracle: byte-identical .o
self-hosted link plan                Rung C   oracle: identical link-plan JSON
```

Forcing-function programs en route (promote each into a PLAN_V3 phase with a
native-required fixture, per the [ROADMAP.md](ROADMAP.md) workflow):

```text
a byte-stream tokenizer  — first program that truly forces R7 (early exit on
                           malformed input) and R6 (parse bytes -> int)
a SHA-256 implementation — forces R4/R5/R6 and validates the hashing the
                           self-hosted importer (Rung A) depends on
```

## 13. Non-goals for initial v3

Initial v3 should not require:

```text
async / concurrency (D3) — depends on self-referential state machines (D1) and
  loops; it cannot even be designed correctly until Pillar 1 and R8 land
a high-performance optimizer (D4)
full DWARF debug info (D6)
full struct-by-value C ABI beyond what FFI needs (D5)
floating point (R10) — not required to self-host
the full agent-native platform — semantic-review-as-a-service, multi-agent
  swarms at scale, and distribution are v4 (SPEC_V4.md)
routing any compilation path through C (emit-c stays a projection)
deleting the Rust compiler — it remains trusted stage-0 and the oracle
```

Some of these arrive later. They are not required before CodeDB can compile
itself.

## 13A. Performance and backend-health gates

Phase 15 has shown that self-hosting is gated not only by semantic correctness but
by build performance and native-backend health. V3 therefore has these gates in
addition to the determinism oracles (see design principles 18 and 19).

### 13A.1 Performance gates

Self-hosting must remain practical enough that agents can iterate on compiler
objects. Required artifact-cache behavior:

```text
interface_hash changes recompile dependents
implementation_hash/body changes recompile the changed symbol and implementation-dependent users
unchanged lowered IR is reused between link planning and object emission
unchanged type layouts are reused across functions and build invocations
native object and link-plan cache keys include all relevant semantic and backend dependencies
```

The mixed compiler computes lowered IR for a function once per root/target/cache
key, then reuses that prepared artifact when emitting objects; link planning may
inspect lowered-IR metadata but must not force a second lowering pass before
object-cache lookup. Required developer diagnostics:

```text
frame-report: per-function frame size, aggregate locals, machine parameter count, max offset
artifact-plan: planned vs cached vs rebuilt compiler artifacts
oracle-localize: first divergent object/migration/history/IR artifact
```

Buffered source ingestion is required once self-hosted tools operate on full source
files; one-byte stdin pumps are allowed only as early probes.

### 13A.2 Backend-health gates

Before extending Rung A much farther, V3 must harden the native backend where
self-hosted compiler code has already exposed correctness hazards. Acceptance gates:

```text
large-frame test: a compiler-shaped function with aggregate locals compiles and runs on supported targets
aggregate-return test: record-returning conditionals and loop accumulators return correctly
hidden-return-slot test: aggregate-returning calls inside loops do not alias loop accumulators or return slots
move-only-sizing test: measuring a move-only string before moving it into a builder is native-equal to evaluator behavior
slot-reuse smoke: liveness-based stack-slot reuse reduces frame size without changing lowered IR semantics
```

Until these pass, source workarounds may remain, but every workaround must be
documented as temporary and covered by a backend bug fixture.

## 14. Open design questions

To settle during v3 implementation, through native acceptance programs:

```text
recursion groups: a by-name reference marker, an explicit recursion form, or a
  recursion-group object — and exactly how is the clique's content hash defined?
generics: monomorphize at lowering — but what is the stable identity of an
  instance, and how does the interface/implementation split apply to it?
drop glue: are conditional and field-granular drops represented as semantic
  objects or as compiler-generated artifacts? (v2 open question, now forced)
how much of the front-end self-hosts before IR-hash equality (Rung A) rather than
  any single missing feature becomes the binding constraint?
error handling: Result + `?` propagation versus scoped break/continue (R7) —
  which lands first, and how does each interact with drop ordering (§7)?
do recursion groups and generic instances become first-class object kinds, and
  how does verify recompute and validate them?
what is the minimum semantic-merge fidelity that supports N concurrent agents
  building the compiler (v3 sets the floor; v4 sets the ceiling)?
front-end spine: which AST/item-table representation is stable enough for probes,
  and does any part become a content-addressed compiler artifact?
canonical writer: what is the minimal object-builder API that expresses all current
  object kinds without falling back to ad hoc payload strings?
dual serialization: how is raw migration serialization mechanically tied to typed
  expression construction from the same AST?
graph canonicalization: how are mixed type/function/extern/recursion groups ordered
  as one source-order-independent item set?
unsupported input: what deterministic unsupported artifact/diagnostic shape should
  the native self-hosted importer emit?
performance: which artifacts are cached before Rung A is complete, and how does the
  cache avoid double lowering during link planning and object emission?
backend health: which v0 backend limitations must be fixed before more compiler
  breadth is accepted?
```

The implementation plan (a future `PLAN_V3.md`, the phase-plan counterpart to
this design track) should answer these incrementally, promoting
[ROADMAP.md](ROADMAP.md) R- and D-items into phases with native-required
fixtures as they are accepted.
