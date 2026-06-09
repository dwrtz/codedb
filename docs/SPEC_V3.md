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

## 11. Acceptance programs

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

## 12. Non-goals for initial v3

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

## 13. Open design questions

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
```

The implementation plan (a future `PLAN_V3.md`, the phase-plan counterpart to
this design track) should answer these incrementally, promoting
[ROADMAP.md](ROADMAP.md) R- and D-items into phases with native-required
fixtures as they are accepted.
