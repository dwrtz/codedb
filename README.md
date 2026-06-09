# codedb

`codedb` is a Rust proof of concept for semantic programming models tracked in
the v0, v1, and v2 docs. Programs are stored as immutable,
content-addressed semantic objects in SQLite, and source files are projections.

The compiler path is object-artifact first:

```text
typed semantic DAG -> lowered IR -> native object files -> link plan -> executable
```

`emit-c` emits a deterministic C projection for debugging and inspection. It is
not the primary native backend.

## Quickstart

Prerequisites:

- Rust and Cargo.
- A host `cc` linker for `codedb build` on the host native target.

Run the shop demo:

```bash
DB=demo.codedb.sqlite
rm -f "$DB" projection.cdb projection.c main.ir.json main.o main.link.json history.ndjson

cargo run -- init "$DB"
cargo run -- import "$DB" examples/shop.cdb
cargo run -- eval "$DB" main
cargo run -- callers "$DB" tax
cargo run -- show "$DB" tax
cargo run -- list "$DB" --json
cargo run -- history "$DB" --json
cargo run -- verify "$DB"
```

Expected `eval main` output is `120`.

Projection and migration walkthrough:

```bash
cargo run -- set-export "$DB" tax public_tax
cargo run -- export-map "$DB"
cargo run -- rename "$DB" tax vat
cargo run -- replace-body "$DB" vat "subtotal * 18 / 100"
cargo run -- create-alias "$DB" vat sales_tax
cargo run -- remove-alias "$DB" vat sales_tax
cargo run -- remove-export "$DB" vat public_tax
cargo run -- export "$DB" --branch main --out projection.cdb
cargo run -- emit-c "$DB" main --out projection.c
cargo run -- replay "$DB" --from-genesis
cargo run -- verify "$DB"
```

After `rename tax vat`, the exported projection renders `vat` at the definition
and call site while preserving the original symbol hash and native ABI symbol.
After `replace-body`, `eval main` changes from `120` to `118`.

Build from structural JSON instead of projection text:

```bash
APPLY_DB=apply-demo.codedb.sqlite
rm -f "$APPLY_DB"

cargo run -- init "$APPLY_DB"
cargo run -- apply "$APPLY_DB" --json examples/shop.apply.json
cargo run -- eval "$APPLY_DB" main
cargo run -- verify "$APPLY_DB"
```

Run the docs/example smoke target with:

```bash
cargo test --test smoke_examples
```

Export and import replayable history:

```bash
REBUILT_DB=rebuilt.codedb.sqlite
rm -f "$REBUILT_DB"

cargo run -- export-history "$DB" --branch main --out history.ndjson
cargo run -- init "$REBUILT_DB"
cargo run -- import-history "$REBUILT_DB" history.ndjson
cargo run -- branches "$REBUILT_DB" --json
cargo run -- verify "$REBUILT_DB"
```

Native artifact inspection:

```bash
cargo run -- emit-ir "$DB" main --out main.ir.json
cargo run -- emit-object "$DB" main --target x86_64-unknown-linux-gnu --out main.o
cargo run -- link-native "$DB" main --target x86_64-unknown-linux-gnu --out main.link.json
cargo run -- build-plan "$DB" main --target x86_64-unknown-linux-gnu --json
```

`codedb build` invokes the host linker, so use it for the host native target.
On Apple Silicon, the default target is `aarch64-apple-darwin`; otherwise the
default target is `x86_64-unknown-linux-gnu`.

```bash
cargo run -- build "$DB" main --out demo-native
./demo-native
echo $?
```

The process exit status is the entry function result.

## Structural Writes

Native ABI identity is separate from display names. `show` prints the stable internal ABI symbol derived from the symbol hash, while `set-export` and `remove-export` manage explicit public ABI names. Renaming `tax` to `vat` does not change either the internal ABI symbol or an explicit export such as `public_tax`.

Structural mutation commands return `applied`, `already_applied`, `conflict`, or
`error`.
Use `--expect-root <root>` on mutation commands when an agent needs to reject
stale writes instead of applying against a branch that has moved.

`codedb apply <db> --json <file>` accepts an atomic structural
`codedb/apply/v1` JSON document with operations such as `create_function`,
`create_external_function`, `rename_symbol`, `move_symbol`,
`replace_function_body`, `create_alias`, and `set_export`.
Function bodies are structural expression JSON objects, so agents do not need
to write projection text to mutate the database. The document `branch` field
selects the branch to mutate and defaults to `main`. See
[docs/APPLY.md](docs/APPLY.md) and [examples/shop.apply.json](examples/shop.apply.json).
Use `--json` on `list`, `show`, `export-map`, and `history` for machine-readable
inspection output.

`codedb patch preview <db> --json <file>` accepts a higher-level
`codedb/semantic-patch/v1` document, matches semantic structure such as
literals, call targets, V2 type definitions, fields, variants, and reference
parameter intent, returns matched hashes and planned structural operations, and
rolls back the typecheck/build-impact preview. Patch results include V2
region/borrow/layout/codegen impact for supported V2 operations such as
`rename_field`, `rename_variant_and_cases`, and
`convert_by_value_param_to_ref`. See
[docs/PATCH.md](docs/PATCH.md).

For operation-by-operation examples, see [docs/MIGRATIONS.md](docs/MIGRATIONS.md).

## Workspace API

Start a local workspace server with:

```bash
cargo run -- serve "$DB" --addr 127.0.0.1:8787
```

The server accepts HTTP `POST /` JSON requests with `method`, `params`, and
optional JSON-RPC `id` fields. Responses use the stable
`codedb/response/v1` envelope. The workspace API exposes read methods:
`workspace.current`, `workspace.branches`, `symbols.list`, `symbols.show`,
`symbols.resolve`, `symbols.callers`, `roots.diff`, `roots.export_projection`,
`modules.list`, `modules.show`, `build.plan`, `build.execute`, `build.artifact_status`, `trace.run`,
`debug.run`, `tests.list`, `tests.run`, `tests.impact`, `history.list`,
`history.bisect`, `provenance.blame_symbol`, `provenance.blame_expr`,
`provenance.blame_type`, `provenance.blame_field`,
`provenance.blame_variant`, `provenance.why_layout`,
`provenance.why_borrow`, `provenance.why_move`, `provenance.why_drop`,
`provenance.why_effect`, `provenance.why_platform_extern`, `why.run`, and
`verify.run`. It also exposes `workspace.branch.create`,
`workspace.branch.fast_forward`, `workspace.branch.delete`,
`workspace.branch.compare`, `ops.apply` for atomic `codedb/apply/v1`
structural writes, and `ops.preview` for rollback-only previews.
`modules.move_symbol` moves a symbol to a different module against an expected root.
`patch.preview`, `patch.apply`, `merge.preview`, and `merge.apply` expose
Milestone C semantic patch and conservative merge workflows through the same
response envelope.

`ops.apply` responses include both `operations` and the older `results` field
for the per-operation records. `trace.run` and `debug.run` return a top-level
workspace `error` envelope when the nested trace/debug report has
`status: "error"`, with the nested diagnostics copied into the envelope.
Branch fast-forward requires `expect_root`; branch delete accepts optional
`expect_root`, and root-bound branch creation should use `from_root` when the
caller wants to pin the exact source root.

## Documentation Map

- [docs/SPEC.md](docs/SPEC.md): v0 design contract for the current implemented
  proof-of-concept architecture.
- [docs/PLAN.md](docs/PLAN.md): v0 implementation plan and current status.
- [docs/SPEC_V1.md](docs/SPEC_V1.md): v1 semantic workspace design track.
- [docs/PLAN_V1.md](docs/PLAN_V1.md): v1 semantic workspace implementation
  roadmap.
- [docs/SPEC_V2.md](docs/SPEC_V2.md): v2 native semantic programming design
  track.
- [docs/PLAN_V2.md](docs/PLAN_V2.md): v2 native semantic programming roadmap
  and phase plan.
- [docs/SPEC_V3.md](docs/SPEC_V3.md): v3 self-hosting semantic programming
  design track.
- [docs/PLAN_V3.md](docs/PLAN_V3.md): v3 self-hosting implementation roadmap
  and phase plan.
- [docs/SPEC_V4.md](docs/SPEC_V4.md): v4 agent-native platform design sketch
  (a horizon, not yet a contract).
- [docs/ROADMAP.md](docs/ROADMAP.md): forward-looking language/compiler gaps
  found by writing real programs against the v2 surface.
- [docs/NATIVE_DONE.md](docs/NATIVE_DONE.md): native-required checklist for v2
  feature completion gates.
- [examples/v2/README.md](examples/v2/README.md): v2 native acceptance program
  index.
- [docs/ARTIFACTS.md](docs/ARTIFACTS.md): object payloads, artifact cache model,
  native object metadata, link plans, and SQLite table roles.
- [docs/MIGRATIONS.md](docs/MIGRATIONS.md): migration cookbook for every
  structural operation and walkthroughs for common edits.
- [docs/APPLY.md](docs/APPLY.md): stable `codedb/apply/v1` JSON schema.
- [docs/PATCH.md](docs/PATCH.md): `codedb/semantic-patch/v1` preview schema.

## Command Reference

Core database commands:

```bash
cargo run -- init <db>
cargo run -- import <db> <file.cdb>
cargo run -- serve <db> --addr 127.0.0.1:8787
cargo run -- export <db> --branch main --out <file.cdb>
cargo run -- export-history <db> --branch main --out <history.ndjson>
cargo run -- import-history <db> <history.ndjson>
cargo run -- replay <db> --from-genesis
cargo run -- verify <db>
```

Inspection and evaluation:

```bash
cargo run -- eval <db> <function-name> [args...]
cargo run -- list <db> [--json]
cargo run -- show <db> <symbol-or-name> [--json]
cargo run -- callers <db> <symbol-or-name>
cargo run -- diff <db> <root-a> <root-b> [--json]
cargo run -- history <db> [--json]
cargo run -- blame-symbol <db> <symbol-or-name> [--branch main] [--json]
cargo run -- blame-expr <db> <expr-hash> [--branch main] [--json]
cargo run -- blame-type <db> <type-or-name> [--branch main] [--json]
cargo run -- blame-field <db> <type-or-name> <field> [--branch main] [--json]
cargo run -- blame-variant <db> <type-or-name> <variant> [--branch main] [--json]
cargo run -- why-layout <db> <type-or-name> [--field <field>] [--target <triple>] [--json]
cargo run -- why-borrow <db> <symbol-or-name> [--body <expr>] [--branch main] [--json]
cargo run -- why-move <db> <symbol-or-name> [--body <expr>] [--branch main] [--json]
cargo run -- why-drop <db> <type-or-name> [--target <triple>] [--json]
cargo run -- why-effect <db> <symbol-or-name> [--branch main] [--json]
cargo run -- why-platform-extern <db> <entry-name> <extern-name> [--target <triple>] [--json]
cargo run -- branches <db> [--json]
cargo run -- branch list <db> [--json]
cargo run -- branch create <db> <name> --from main [--json]
cargo run -- branch create <db> <name> --from-root <root> [--json]
cargo run -- branch compare <db> <branch-a> <branch-b> [--json]
cargo run -- branch fast-forward <db> <target> <source> --expect-root <root> [--json]
cargo run -- branch delete <db> <name> [--expect-root <root>] [--json]
cargo run -- export-map <db> [--json]
```

Semantic tests:

```bash
cargo run -- test <db> [--list] [--json]
cargo run -- create-test <db> <name> --entry <function> [--arg <value>] --expect-i64 <value> [--category behavior|projection|export] [--expect-root <root>] [--json]
cargo run -- delete-test <db> <name> [--expect-root <root>] [--json]
cargo run -- test-impact <db> <old-root> <new-root> [--json]
```

Structural mutations:

```bash
cargo run -- apply <db> --json <apply.json>
cargo run -- rename <db> <old-name> <new-name> [--expect-root <root>] [--json]
cargo run -- replace-body <db> <name> <expr> [--expect-root <root>] [--json]
cargo run -- change-signature <db> <name> "<signature>" [--expect-root <root>] [--json]
cargo run -- delete-symbol <db> <name> [--force] [--expect-root <root>] [--json]
cargo run -- create-alias <db> <name> <alias> [--expect-root <root>] [--json]
cargo run -- remove-alias <db> <name> <alias> [--expect-root <root>] [--json]
cargo run -- module list <db> [--json]
cargo run -- module show <db> <module> [--json]
cargo run -- module move-symbol <db> <symbol-or-name> <module> --expect-root <root> [--json]
cargo run -- set-export <db> <name> <exported-name> [--expect-root <root>] [--json]
cargo run -- remove-export <db> <name> <exported-name> [--expect-root <root>] [--json]
```

Projection and native artifacts:

```bash
cargo run -- emit-c <db> <function-name> --out <file.c>
cargo run -- emit-ir <db> <function-name> --out <file.json>
cargo run -- emit-object <db> <function-name> --target <triple> --out <file.o>
cargo run -- link-native <db> <entry-name> --target <triple> --out <file.json>
cargo run -- build-plan <db> <entry-name> --target <triple> --json
cargo run -- build <db> <entry-name> --target <triple> --out <executable>
```

The integration suite in [tests/smoke_examples.rs](tests/smoke_examples.rs),
[tests/demo.rs](tests/demo.rs), and [tests/corruption.rs](tests/corruption.rs)
exercises these command examples, including docs smoke flows, object artifact
reuse, link plan determinism, history import/export, structural apply, and
verification failures.
