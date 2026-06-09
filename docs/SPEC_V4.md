# SPEC_V4.md — CodeDB Agent-Native Programming Platform

Status: Sketch / pre-draft (intentionally lighter than SPEC_V3)
Scope: the epoch after self-hosting; recorded now to scope v3, not to commit v4

## 0. Why a sketch and not a contract

The v0, v1, and v2 specs are contracts because their features are discoverable
and buildable now. The v3 spec is a contract for the same reason. V4 is far
enough out that a detailed contract would be false precision, and over-specifying
it would cut against CodeDB's empirical discipline: a feature is not real until a
program forces it, and v4's real content will be *discovered* while building v3.

This document therefore does three things and no more:

```text
1. names the next epoch's thesis
2. fences the horizon — holds the things v3 must NOT absorb
3. records the open questions that v3 will inform
```

[ROADMAP.md](ROADMAP.md) remains the place where concrete forward-looking gaps
accrue with IDs and discovery context. SPEC_V4 is the epoch frame above it, not a
replacement for it. Its most useful near-term role is as a scoping fence for v3:
it is where the temptations that would bloat v3 — the full agent product,
distribution — are parked on purpose.

## 1. Thesis

V3 makes the language real and self-hosting, with a *minimum* agent-editing
spine. V4 makes the agent-native editing the product.

```text
CodeDB v4 is the platform where agents author, review, merge, and distribute
programs with proof-carrying change-impact. The unique powers of the
content-addressed substrate — provable change semantics, complete provenance,
fail-closed verification, optimistic concurrent structural editing — become the
point, not the plumbing.
```

This is the other half of the goal that v3 deliberately holds back. The
"language-real versus agent-native" question is resolved by sequencing the two
as epochs: v3 = language-real (self-hosting); v4 = agent-native (the platform).

## 2. Relationship to v3

V3 keeps the agent layer at the minimum that lets a handful of agents build the
compiler-in-CodeDB concurrently (semantic merge, `--expect-root` concurrency,
proof-carrying receipts; see [SPEC_V3.md](SPEC_V3.md) §9). V4 promotes that layer
from infrastructure to product: from "good enough to build one compiler" to "the
reason teams and agent fleets choose CodeDB over a file-based toolchain."

The self-hosted compiler from v3 is v4's proof exhibit: because the compiler is
itself content-addressed objects, its own provenance, incremental rebuild,
semantic diff, and proof-carrying review already apply to it. V4 generalizes
that experience from the compiler to arbitrary agent-authored programs.

## 3. Candidate pillars

Leading thesis (Pillar A), with alternatives recorded honestly because v4 is a
sketch:

```text
A. Agent-native editing at scale (leading)
   semantic merge at scale; multi-agent concurrent editing as a first-class
   workflow; proof-carrying change review as a product surface (an agent proposes
   a structural change and a reviewer — human or agent — sees typecheck, borrow/
   effect/capability deltas, build impact, and a semantic diff before commit);
   the why/provenance graph as a queryable knowledge base; the HTTP / MCP
   workspace API promoted from tool to platform.

B. Distribution (D7)
   content-addressed packages across databases; the object store as a registry;
   cross-database dependency resolution. A natural extension once the language is
   self-hosting and agent-edited — the content-addressed store is already the
   right shape for it.

C. Alternative framing — the runtime frontier (D3 + D1)
   async / concurrency and self-referential state machines. Recorded as a
   candidate, but this reads as a continued *language* epoch rather than a
   *platform* one; it is more likely v5 than v4.
```

## 4. The horizon fence (what v4 inherits as still-deferred)

These remain deferred at v4's start; some may be v5. They are not v4's identity:

```text
async / concurrency (D3)
high-performance optimizer (D4)
full DWARF debug info (D6)
full struct-by-value C ABI (D5)
floating point (R10), if still unbuilt
```

V4 is not a rewrite, not a new substrate, and not where the language gets
"finished" (that is v3 plus ongoing language work). V4 is the platform layer over
a real, self-hosting language.

## 5. Open questions v3 will inform

```text
how good must semantic merge be for N concurrent agents? v3 sets the floor by
  building one compiler; v4 sets the ceiling.
does the content-addressed object store become a public/shared registry, and
  what is the trust and identity model for published objects?
is the platform single-tenant (a team's database) or multi-tenant / hosted?
does proof-carrying review generalize cleanly beyond the compiler to arbitrary
  agent-authored programs, or does it need per-domain extension?
what does a human reviewer's surface over agent-authored structural changes look
  like, given that the source is objects and the text is a projection?
```

## 6. What would promote this sketch to a draft

V4 becomes a contract (Draft 1.0) when v3 has produced the evidence:

```text
the self-hosted compiler exists and is built through structural edits
semantic merge has been exercised by concurrent agents on a real codebase
proof-carrying receipts have been used in anger and their gaps are known
the binding constraints on multi-agent editing are observed, not guessed
```

Until then this sketch's job is to keep v3 focused — every time v3 is tempted to
build "a bit more of the agent product," that work belongs here.
