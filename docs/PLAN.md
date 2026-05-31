# CodeDB Implementation Plan

This plan starts from the first working proof of concept and turns it into a sturdier compiler/database prototype. The sequencing is intentionally conservative: make the working core easier to change, then harden semantics, then expand the language and tooling.

## Phase 1: Modularize the Working Core

Goal: split the current single library into clear ownership boundaries without changing behavior.

Deliverables:

- Move SQLite object storage, schema setup, cache writes, and hash verification into `store`.
- Move type hashes, function signatures, and type checking into `types`.
- Move expression data structures, parser output, rendering, dependency traversal, and evaluator helpers into `expr`.
- Move migration operations, history hashing, replay, and branch updates into `migrations`.
- Move C backend generation and forbidden-runtime checks into `backend_c`.
- Move semantic diff and verification into `diff` and `verify`.
- Keep the CLI thin: argument parsing plus calls into the library.

Acceptance checks:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- Existing demo commands produce the same results and root hashes where the semantic payloads are unchanged.

## Phase 2: First-Class Migration Outcomes

Goal: make migration idempotence explicit instead of implicit.

Deliverables:

- Introduce `MigrationOutcome::Applied`, `MigrationOutcome::AlreadyApplied`, and `MigrationOutcome::Conflict`.
- Represent precondition and postcondition checks as typed values rather than ad hoc JSON construction.
- Make every structural operation return one of the three outcomes.
- Add tests for retrying `rename_symbol`, `replace_function_body`, `create_alias`, and `delete_symbol`.
- Ensure conflicts do not update branch pointers, migration rows, history rows, indexes, or caches.

Acceptance checks:

- Retrying a completed migration returns `already_applied`.
- Applying a migration against a divergent root returns `conflict`.
- Replay remains deterministic.
- `verify` distinguishes bad history links from semantic conflicts.

## Phase 3: Verification and Corruption Tests

Goal: make `codedb verify` a trustworthy guardrail for the object store and materialized indexes.

Deliverables:

- Add targeted corruption tests for object payload tampering, bad hashes, missing objects, bad root indexes, bad dependency indexes, bad history hashes, and invalid cache entries.
- Rebuild expected indexes in memory and compare them to SQLite materialized tables.
- Verify every object edge can be recomputed from payload references.
- Verify every cached C projection still passes the forbidden-runtime scan.
- Make failure classes stable enough for tests and CLI consumers.

Acceptance checks:

- Each intentional corruption test fails with the expected failure class.
- Clean databases from all examples pass verification.
- Verification remains read-only except for initialization of missing built-in type objects in empty databases.

## Phase 4: Structural Operation API

Goal: make agent-first writes practical without relying on projection text.

Deliverables:

- Add `codedb apply <db> --json <file>` for structural operations.
- Define stable JSON schemas for `create_function`, `rename_symbol`, `replace_function_body`, `change_function_signature`, `delete_symbol`, and `create_alias`.
- Return machine-readable summaries containing old root, new root, migration hash, history hash, type-check result, and compile impact.
- Add JSON output mode for relevant CLI commands.

Acceptance checks:

- The shop demo can be built entirely from JSON operations.
- JSON operations replay to the same final root.
- Invalid operations fail before root creation.

## Phase 5: Language Surface Expansion

Goal: add a small amount of language expressiveness that directly exercises the DAG model.

Deliverables:

- Add `let` expressions with typed bindings.
- Add `unit` literals and unit-returning functions.
- Add unary operators for integer negation and boolean not.
- Add expression DAG reuse tests where shared subexpressions produce identical hashes.
- Keep projection syntax canonical and deterministic.

Acceptance checks:

- Type errors in `let`, unit returns, and unary ops are rejected.
- Export/import remains stable for supported syntax.
- Evaluator and C backend support the expanded expression set without heap allocation.

## Phase 6: Backend Execution Support

Goal: make no-runtime artifacts easier to inspect and execute.

Deliverables:

- Add `codedb emit-c-harness <db> <function-name> --out <file>` for an external test harness.
- Add `codedb compile-c <db> <function-name> --out <binary>` when a local C compiler is available.
- Cache harness and compiled artifact metadata separately from the target-language C projection.
- Add platform-aware tests that compile and run generated C when `cc` is present.

Acceptance checks:

- Generated C remains free of forbidden runtime calls.
- Harness code is clearly separated from target-language output.
- `codedb_main` from `examples/shop.cdb` compiles and returns `120`.

## Phase 7: Incremental Compilation Semantics

Goal: make compile impact more than a diff message.

Deliverables:

- Formalize interface hashes and implementation hashes.
- Track direct and transitive dependents for each function.
- Add cache invalidation planning for metadata-only, implementation-only, and interface changes.
- Surface compile impact in migration summaries and diffs.

Acceptance checks:

- Rename is classified as metadata-only.
- Body replacement invalidates the changed function implementation artifact only.
- Signature changes identify direct and transitive dependents.

## Phase 8: Branch and History Export

Goal: make database rebuild independent from copying the SQLite file.

Deliverables:

- Add `codedb export-history <db> --branch main --out history.ndjson`.
- Add `codedb import-history <db> history.ndjson`.
- Ensure exported migrations are canonical and deterministic.
- Add branch pointer inspection commands.

Acceptance checks:

- A fresh database rebuilt from exported history reaches the same root and history head.
- History import detects missing, reordered, or tampered migrations.
- `verify` validates imported histories without requiring source projections.

## Phase 9: Documentation and Examples

Goal: keep the prototype understandable as the implementation grows.

Deliverables:

- Document the object kinds and payload shapes currently supported.
- Add a migration cookbook for each structural operation.
- Add example walkthroughs for rename, body replacement, aliasing, and signature change.
- Add an architecture overview showing how SQLite tables, object payloads, indexes, and caches relate.

Acceptance checks:

- A new contributor can run the demo from README alone.
- Each major CLI command has a tested example.
- Documentation stays aligned with current behavior.

## Near-Term Recommendation

Do Phase 1 and Phase 2 next. The proof of concept works, but most future work will be cheaper after the core is modular and migration outcomes are explicit. Phase 3 should follow immediately after, because stronger verification will keep later language and backend changes honest.
