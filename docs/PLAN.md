# CodeDB Implementation Plan

Status: Draft 0.3  
Scope: roadmap from the current Rust proof of concept toward an incremental object-code compiler/database prototype

## Direction

The current proof of concept already demonstrates the core idea: programs are stored as immutable, content-addressed semantic objects, while source text and C output are projections. The next implementation goal is to make that distinction explicit in the codebase and prevent the C emitter from becoming the compiler architecture.

The target compiler pipeline is:

```text
ProgramRoot
  -> FunctionDef(symbol_hash, function_sig_hash, typed_body_expr_hash)
  -> LoweredFunctionIR
  -> per-function or per-codegen-unit object artifact
  -> LinkPlan
  -> executable / ELF / shared object
```

The C emitter remains useful, but it should be treated as:

```text
ProgramRoot -> C source projection / debug artifact / bootstrap inspection path
```

It is not the primary long-term backend. Incremental compilation should be driven by symbol, interface, implementation, dependency, target, ABI, and codegen hashes, not by whole-root C source text.

## Current implementation baseline

The repository currently has:

- A Rust CLI and library crate.
- SQLite-backed immutable object storage.
- Canonical JSON hashing.
- Program roots, symbol births, function signatures, function definitions, expression DAG objects, branches, migrations, histories, materialized indexes, and cache rows.
- A small typed expression language with integer, boolean, call, binary, and conditional expressions.
- A reference evaluator for tests and debugging.
- A C projection emitter for pure `i64` / `bool` / `unit` functions.
- Cache entries for rendered sources, typed expressions, dependency sets, interface hashes, implementation hashes, and C projection text.
- `artifact_bytes` support in the cache schema, although the current C path mostly writes text artifacts.

## Phase 1: Name the Backend Boundary Correctly

Goal: separate projection generation from compilation artifacts before adding more backend features.

Deliverables:

- Rename the conceptual role of `src/backend_c.rs` to “C projection” in docs, CLI help, cache metadata, and tests.
- Keep the CLI command `emit-c`, but document it as a projection/debug command.
- Add a new backend module boundary, for example `src/backend/mod.rs`, even if it initially contains only data structures and traits.
- Define artifact kinds as typed values rather than ad hoc strings:
  - `canonical_source`
  - `c_projection`
  - `typed_expression`
  - `function_dependency_set`
  - `interface_hash`
  - `implementation_hash`
  - `lowered_ir`
  - `object_file`
  - `link_plan`
  - `executable`
- Make “backend” mean a compiler backend that consumes typed/lowered IR and emits binary artifacts. C remains a projection unless a future C compiler adapter explicitly compiles that projection as a toolchain step.

Files likely touched:

- `docs/SPEC.md`
- `docs/PLAN.md`
- `src/backend_c.rs`
- `src/lib.rs`
- `src/main.rs`
- `src/store.rs`
- `tests/demo.rs`

Acceptance checks:

- Existing `emit-c` behavior remains deterministic.
- Existing demo tests still pass.
- New docs state that C source is not the primary compiler architecture.
- Cache rows distinguish `c_projection` from native object artifacts.

## Phase 2: First-Class Migration Outcomes

Goal: make migration idempotence explicit instead of implicit.

Deliverables:

- Introduce typed outcomes:
  - `MigrationOutcome::Applied`
  - `MigrationOutcome::AlreadyApplied`
  - `MigrationOutcome::Conflict`
- Represent preconditions and postconditions as typed values rather than ad hoc JSON construction.
- Make every structural operation return one of the three outcomes.
- Ensure conflicts do not update branch pointers, migration rows, history rows, indexes, or caches.
- Return structured summaries that include semantic impact and build impact.

Files likely touched:

- `src/migrations.rs`
- `src/lib.rs`
- `src/main.rs`
- `tests/demo.rs`

Acceptance checks:

- Retrying a completed rename returns `already_applied`.
- Applying a migration against a divergent root returns `conflict`.
- Replay remains deterministic.
- `verify` distinguishes bad history links from semantic conflicts.

## Phase 3: Build Impact as Data

Goal: make compile impact a real build plan, not just text in a diff.

Deliverables:

- Define `BuildImpact` values:
  - `metadata_only`
  - `relink_only`
  - `recompile_symbols`
  - `recompile_dependents`
  - `full_rebuild`
- Add a build planner that compares old and new roots and returns affected symbols and artifact kinds.
- Track direct and transitive dependents for each function using the dependency index.
- Classify changes using symbol hash, function signature hash, function definition hash, body expression hash, and dependency set.
- Surface build impact in migration summaries, diffs, and JSON output.

Files likely touched:

- `src/diff.rs`
- `src/migrations.rs`
- `src/store.rs`
- new `src/build_plan.rs`

Acceptance checks:

- Rename is classified as `metadata_only`.
- Alias creation/removal is classified as `metadata_only` unless export maps are affected.
- Body replacement with unchanged signature recompiles the changed symbol object only and then relinks final binaries that include it.
- Signature change identifies direct and transitive dependents.
- Deleting an unused symbol is `relink_only` for binaries that included it.

## Phase 4: Cache Key and Artifact Model Hardening

Goal: make cache keys precise enough for object-code artifacts.

Deliverables:

- Replace the current string-built cache key payload with a typed `CacheKeyInput` structure serialized canonically.
- Include all artifact-affecting inputs:
  - artifact kind
  - input object hash
  - dependency interface hashes
  - dependency implementation hashes when required by the artifact kind
  - backend ID
  - target triple
  - ABI tag
  - relocation model
  - code model
  - optimization level
  - compiler version
  - pipeline version
  - runtime sentinel, normally `runtime:none`
- Use `artifact_bytes` for native objects and executables.
- Store structured metadata in `artifact_json`, including symbol hash, exported ABI names, object format, target triple, and dependency closure.
- Add cache lookup helpers, not only cache write helpers.

Files likely touched:

- `src/store.rs`
- `schema.sql`
- new `src/artifact.rs`
- `src/verify.rs`

Acceptance checks:

- The same semantic function compiled for two target triples produces different cache keys.
- A rename does not invalidate native object cache keys when ABI export metadata is unchanged.
- A body-only change invalidates only the changed function object and downstream link products.
- `verify` catches cache rows whose keys do not match their stored metadata.

## Phase 5: Stable ABI Symbol Names and Export Maps

Goal: decouple native symbol names from human display names.

Initial implementation status:

- `ProgramRoot` has an explicit `exports` map from symbol identity to public ABI names.
- Internal ABI symbols are derived from stable symbol hashes as `codedb_<short_symbol_hash>`.
- `set-export` and `remove-export` migrations update export metadata without renaming symbols.
- Build impact classifies export-map changes as `relink_only` and includes link artifacts.
- `show` and `export-map` expose internal ABI names and explicit public exports for inspection.

Deliverables:

- Define internal ABI symbol names from stable identity, for example `codedb_<short_symbol_hash>`.
- Define an explicit export map for friendly public symbols.
- Keep display names and parameter names as projection metadata.
- Decide how public ABI names change on rename:
  - internal symbols should not change;
  - exported friendly names may change only if an export-map migration says so.
- Update C projection docs to allow friendly names there while forbidding friendly names in native object identity.

Files likely touched:

- `src/model.rs`
- `src/backend_c.rs`
- new `src/abi.rs`
- `src/store.rs`
- `docs/SPEC.md`

Acceptance checks:

- Rename does not change native ABI symbol names.
- Rename may change C projection names because C projection is human-facing text.
- Export-map changes are explicitly classified and included in link plans.
- Native object cache keys do not include display names.

## Phase 6: Lowered IR Scaffold

Goal: introduce a target-independent lowering layer between typed expression DAGs and codegen.

Deliverables:

- Add a compact lowered function IR for the v0 language.
- Lower each `FunctionDef` independently.
- Include explicit parameter slots, call targets by `symbol_hash`, typed operations, return operation, and traps for checked operations where needed.
- Store lowered IR as a content-addressed artifact or cache entry.
- Add `codedb emit-ir <db> <function-name> --out <file>` for inspection.

Files likely touched:

- new `src/lowering.rs`
- new `src/backend/mod.rs`
- `src/types.rs`
- `src/store.rs`
- `src/main.rs`

Acceptance checks:

- Lowered IR for unchanged function definitions has the same hash across renames.
- Calls in lowered IR use symbol hashes or resolved ABI symbols, never display names.
- Lowered IR verifies before codegen.
- Hash-pruned lowering skips unchanged function definitions.

## Phase 7: Native Object Backend v0

Goal: compile typed/lowered functions into binary object artifacts without routing through whole-program C source.

Deliverables:

- Add a backend trait that can produce an `object_file` artifact from a lowered function or codegen unit.
- Choose an initial implementation route:
  - direct ELF relocatable object writer for the tiny v0 instruction set, or
  - Cranelift/LLVM-backed object emission behind the same artifact interface.
- Support at least Linux x86-64 ELF relocatable object files for pure `i64` / `bool` functions.
- Generate one object per function or one deterministic codegen unit per small dependency cluster.
- Emit relocations for calls between CodeDB symbols.
- Cache object bytes in `compile_cache.artifact_bytes`.

Files likely touched:

- new `src/backend_native.rs` or `src/backend/elf.rs`
- new `src/artifact.rs`
- `src/store.rs`
- `src/main.rs`
- `tests/native.rs`

Acceptance checks:

- A body-only change recompiles one object file.
- A rename does not recompile any object file.
- Signature change recompiles the changed function and affected dependents.
- Object artifacts expose stable ABI symbols derived from symbol identity.
- Object cache entries are reusable across branch roots when the function implementation and target inputs are identical.

## Phase 8: Link Plan and ELF Output

Goal: make linking explicit, cached, and incremental.

Deliverables:

- Define `LinkPlan` as a deterministic artifact containing:
  - target triple
  - entry symbol
  - object artifact hashes
  - object symbol definitions
  - relocation requirements
  - export map
  - external symbols
  - link options
- Add `codedb link-native <db> <entry-name> --out <file>`.
- Add `codedb build <db> <entry-name> --target <triple> --out <file>` as the high-level command.
- Cache executable bytes where practical, or cache link metadata if platform linking is delegated to an external linker.
- Verify link plans can be recomputed from root and artifact metadata.

Files likely touched:

- new `src/link.rs`
- new `src/build.rs`
- `src/store.rs`
- `src/main.rs`
- `src/verify.rs`

Acceptance checks:

- Relinking after a single body change reuses all unchanged object artifacts.
- Rename only regenerates projections and maybe export-map/link metadata if configured to expose friendly names.
- Link plan ordering is deterministic.
- Rebuilding from exported history reaches the same link plan for the same target inputs.

## Phase 9: Verification and Corruption Tests

Goal: make `codedb verify` a trustworthy guardrail for the object store, indexes, caches, and native artifacts.

Deliverables:

- Add targeted corruption tests for:
  - object payload tampering
  - bad hashes
  - missing objects
  - bad root indexes
  - bad dependency indexes
  - bad history hashes
  - invalid cache entries
  - malformed artifact metadata
  - mismatched object bytes hash
  - C projection forbidden runtime calls
  - link plans that reference missing objects or stale artifacts
- Rebuild expected indexes in memory and compare them to SQLite materialized tables.
- Verify every object edge can be recomputed from payload references.
- Verify every cached object artifact matches its metadata and key inputs.

Files likely touched:

- `src/verify.rs`
- `tests/demo.rs`
- new `tests/corruption.rs`
- new `tests/native.rs`

Acceptance checks:

- Each intentional corruption test fails with the expected failure class.
- Clean databases from all examples pass verification.
- Verification remains read-only except initialization of missing built-in type objects in empty databases.

## Phase 10: Structural Operation API

Goal: make agent-first writes practical without relying on projection text.

Deliverables:

- Add `codedb apply <db> --json <file>` for structural operations.
- Define stable JSON schemas for:
  - `create_function`
  - `rename_symbol`
  - `replace_function_body`
  - `change_function_signature`
  - `delete_symbol`
  - `create_alias`
  - `set_export`
  - `remove_export`
- Add JSON output mode for relevant CLI commands.
- Return machine-readable summaries containing old root, new root, migration hash, history hash, type-check result, semantic impact, and build impact.

Files likely touched:

- `src/main.rs`
- `src/lib.rs`
- `src/migrations.rs`
- new `src/api.rs`

Acceptance checks:

- The shop demo can be built entirely from JSON operations.
- JSON operations replay to the same final root.
- Invalid operations fail before root creation.
- Build impact is returned as structured data.

## Phase 11: Language Surface Expansion

Goal: add a small amount of language expressiveness that exercises the DAG model and backend boundaries.

Deliverables:

- Add `let` expressions with typed bindings.
- Add explicit `unit` literals and unit-returning functions.
- Add unary operators for integer negation and boolean not.
- Add expression DAG reuse tests where shared subexpressions produce identical hashes.
- Keep projection syntax canonical and deterministic.
- Ensure lowering and native backend support each new expression form before calling it complete.

Files likely touched:

- `src/expr.rs`
- `src/types.rs`
- `src/backend_c.rs`
- `src/lowering.rs`
- native backend files
- `tests/demo.rs`

Acceptance checks:

- Type errors in `let`, unit returns, and unary ops are rejected.
- Export/import remains stable for supported syntax.
- Evaluator, C projection, lowering, and native object backend agree on behavior.
- No new feature requires implicit heap allocation.

## Phase 12: Branch and History Export

Goal: make database rebuild independent from copying the SQLite file.

Deliverables:

- Add `codedb export-history <db> --branch main --out history.ndjson`.
- Add `codedb import-history <db> history.ndjson`.
- Ensure exported migrations are canonical and deterministic.
- Add branch pointer inspection commands.
- Allow rebuild of cache/artifact state from roots plus target options, while treating cached artifacts as disposable.

Files likely touched:

- `src/migrations.rs`
- `src/main.rs`
- `src/store.rs`
- `src/verify.rs`

Acceptance checks:

- A fresh database rebuilt from exported history reaches the same root and history head.
- History import detects missing, reordered, or tampered migrations.
- `verify` validates imported histories without requiring source projections.
- Native artifacts can be regenerated after history import.

## Phase 13: Documentation and Examples

Goal: keep the prototype understandable as the implementation grows.

Deliverables:

- Document the object kinds and payload shapes currently supported.
- Add an artifact model document showing how typed DAGs, lowered IR, object files, link plans, and projections relate.
- Add a migration cookbook for each structural operation.
- Add example walkthroughs for rename, body replacement, aliasing, signature change, export-map change, and native build.
- Add an architecture overview showing how SQLite tables, object payloads, indexes, and caches relate.

Files likely touched:

- `README.md`
- `docs/SPEC.md`
- `docs/PLAN.md`
- new `docs/ARTIFACTS.md`
- new `docs/MIGRATIONS.md`

Acceptance checks:

- A new contributor can run the demo from README alone.
- Each major CLI command has a tested example.
- Documentation stays aligned with current behavior.
- The docs never imply that C source is the primary native backend.

## Near-Term Recommendation

Do Phases 1 through 4 next. They are smaller than the full native backend, but they lock in the crucial architecture:

```text
C projection is a projection.
Native object artifacts are compiler outputs.
Incremental build impact is structured data.
Cache keys are exact enough for binary artifacts.
```

After that, build the lowered IR scaffold before expanding the language surface. A richer language before a real artifact boundary will make the eventual native backend harder to reason about.
