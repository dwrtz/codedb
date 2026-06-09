# compiler/ - Self-Hosted CodeDB Compiler (Skeleton)

Status: v3 self-hosting skeleton (see docs/SPEC_V3.md and docs/PLAN_V3.md)

This tree will hold the CodeDB compiler expressed as CodeDB `.cdb` objects — the
v3 self-hosting target. Each stage is a ladder rung that must reproduce the
trusted Rust compiler's artifact under a determinism oracle (the self-hosting
completion rule, docs/SPEC_V3.md §4).

The only compilation path is the native one. Projections (`emit-c`, source text)
are views, never a stage. Self-hosting is staged at the lowered-IR seam: until a
rung is self-hosted, the Rust implementation of that stage runs (a mixed
compiler), and the CodeDB and Rust stages meet at a deterministic,
hash-comparable artifact.

| Dir | Rung | Stage | Determinism oracle |
| --- | --- | --- | --- |
| `eval/` | 0 | reference evaluator (warm-up, off the compile path) | result == Rust evaluator on the corpus |
| `front/` | A | importer -> typecheck -> borrow/effect/move/drop -> layout -> lowering | IR-hash == Rust front-end |
| `backend/` | B | lowered IR -> native object (`.o`) | bytes == Rust emitter |
| `link/` | C | object set -> link plan -> executable | JSON == Rust linker driver |

Build order: V3.2 (eval) -> V3.4 (front, mixed compiler) -> V3.5 (backend, link).
No rung is counted self-hosted without its oracle. The Rust compiler is retained
as trusted stage-0 and the oracle; self-hosting reproduces it, it does not delete
it.

These directories are skeletons. Stage objects are added as the language features
each rung needs (docs/PLAN_V3.md phases) land.
