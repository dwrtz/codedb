# Migration Cookbook

Migrations are semantic operations over a `ProgramRoot`, not text edits. Every
mutation command records a migration row and advances the `main` branch only
when the operation is applied successfully.

Each mutation returns one of:

```text
applied
already_applied
conflict
```

Use `--expect-root <root>` when an agent wants stale writes to fail instead of
applying against a branch that has moved. Conflicts do not update branch
pointers, migration rows, history rows, indexes, or caches.

## Setup

Most examples use the shop program:

```bash
DB=demo.codedb.sqlite
rm -f "$DB"
cargo run -- init "$DB"
cargo run -- import "$DB" examples/shop.cdb
cargo run -- eval "$DB" main
```

Expected output:

```text
120
```

Get root hashes for guarded writes and diffs:

```bash
cargo run -- show "$DB" tax
cargo run -- history "$DB" --json
```

## Create Function

There is no separate `create-function` CLI command. Create functions through
`import` or structural `apply`.

```bash
APPLY_DB=apply-demo.codedb.sqlite
rm -f "$APPLY_DB"
cargo run -- init "$APPLY_DB"
cargo run -- apply "$APPLY_DB" --json examples/shop.apply.json
cargo run -- eval "$APPLY_DB" main
```

Single-operation apply document:

```json
{
  "schema": "codedb/apply/v1",
  "operations": [
    {
      "kind": "create_function",
      "name": "answer",
      "birth_seed": "cookbook-answer",
      "params": [],
      "return_type": "i64",
      "body": { "kind": "literal_i64", "value": "42" }
    }
  ]
}
```

Effects:

- Creates `SymbolBirth`, `FunctionSignature`, typed `Expression`,
  `FunctionDef`, and a new `ProgramRoot`.
- Type-checks before the root is committed.
- Build impact includes the created symbol and downstream link artifacts.

## Rename Symbol

```bash
cargo run -- rename "$DB" tax vat
cargo run -- show "$DB" vat
cargo run -- export "$DB" --branch main --out projection.cdb
```

JSON:

```json
{ "kind": "rename_symbol", "name": "tax", "new_name": "vat" }
```

Effects:

- Updates preferred display-name metadata in `ProgramRoot.names`.
- Does not change `symbol_hash`, `FunctionDef`, body hash, or internal native
  ABI symbol.
- Usually has `metadata_only` build impact.
- Regenerates source and C projections because projections are human-facing.
- Reusing the same rename returns `already_applied`.

## Replace Function Body

```bash
cargo run -- replace-body "$DB" vat "subtotal * 18 / 100"
cargo run -- eval "$DB" main
```

Expected output after replacing the tax body:

```text
118
```

JSON:

```json
{
  "kind": "replace_function_body",
  "name": "vat",
  "body": {
    "kind": "binary",
    "op": "/",
    "left": {
      "kind": "binary",
      "op": "*",
      "left": { "kind": "param_name", "name": "subtotal" },
      "right": { "kind": "literal_i64", "value": "18" }
    },
    "right": { "kind": "literal_i64", "value": "100" }
  }
}
```

Effects:

- Re-type-checks the replacement body against the current signature.
- Creates a new typed expression DAG and `FunctionDef` for the same symbol.
- Keeps the callable interface stable when the signature is unchanged.
- Recompiles the changed function object and relinks outputs that include it;
  unchanged caller objects can be reused.

## Change Function Signature

Use a small identity function for the simplest signature walkthrough:

```bash
SIG_DB=signature-demo.codedb.sqlite
SIG_SRC=signature-demo.cdb
rm -f "$SIG_DB" "$SIG_SRC"
printf 'fn id(x: i64) -> i64 = x\n' > "$SIG_SRC"
cargo run -- init "$SIG_DB"
cargo run -- import "$SIG_DB" "$SIG_SRC"
cargo run -- change-signature "$SIG_DB" id "(x: bool) -> bool"
cargo run -- eval "$SIG_DB" id true
```

JSON:

```json
{
  "kind": "change_function_signature",
  "name": "id",
  "params": [{ "name": "x", "type": "bool" }],
  "return_type": "bool"
}
```

Effects:

- Creates a new `FunctionSignature` and `FunctionDef` for the same symbol.
- Re-type-checks the function body under the new parameter types.
- Re-checks reachable callers because their call expressions depend on the
  callee interface.
- Build impact is `recompile_dependents`.

## Delete Symbol

Create an unused symbol and delete it:

```bash
DELETE_DB=delete-demo.codedb.sqlite
DELETE_SRC=delete-demo.cdb
rm -f "$DELETE_DB" "$DELETE_SRC"
printf 'fn unused() -> i64 = 1\n\nfn main() -> i64 = 2\n' > "$DELETE_SRC"
cargo run -- init "$DELETE_DB"
cargo run -- import "$DELETE_DB" "$DELETE_SRC"
cargo run -- delete-symbol "$DELETE_DB" unused
cargo run -- show "$DELETE_DB" main
```

JSON:

```json
{ "kind": "delete_symbol", "name": "unused", "force": false }
```

Effects:

- Removes the symbol, names, parameter names, and exports from the new root.
- Rejects deletion when live dependencies still call the symbol. `--force`
  skips the explicit live-caller precheck, but root type-checking still prevents
  a committed root with dangling calls.
- Deleting an unused symbol is typically `relink_only` for outputs that included
  it.

## Create Alias

```bash
cargo run -- create-alias "$DB" vat sales_tax
cargo run -- show "$DB" sales_tax
```

JSON:

```json
{ "kind": "create_alias", "name": "vat", "alias": "sales_tax" }
```

Effects:

- Adds a non-preferred `ProgramRoot.names` binding to the same symbol.
- Does not change semantic implementation or native ABI identity.
- Usually has `metadata_only` build impact.

## Remove Alias

```bash
cargo run -- remove-alias "$DB" vat sales_tax
```

JSON:

```json
{ "kind": "remove_alias", "name": "vat", "alias": "sales_tax" }
```

Effects:

- Removes the alias binding.
- Keeps the preferred name and symbol identity unchanged.
- Retrying the removal returns `already_applied`.

## Set Export

```bash
cargo run -- set-export "$DB" vat public_tax
cargo run -- export-map "$DB"
cargo run -- link-native "$DB" main --target x86_64-unknown-linux-gnu --out main.link.json
```

JSON:

```json
{ "kind": "set_export", "name": "vat", "exported_name": "public_tax" }
```

Effects:

- Adds an explicit public ABI name in `ProgramRoot.exports`.
- Does not change display names, `FunctionDef`, lowered IR, or object-file cache
  identity.
- Link plans include the export map for reachable symbols.
- Build impact is `relink_only`.

Export names must be native ABI identifiers and may not be reserved names such
as `main` or C keywords.

## Remove Export

```bash
cargo run -- remove-export "$DB" vat public_tax
cargo run -- export-map "$DB"
```

JSON:

```json
{ "kind": "remove_export", "name": "vat", "exported_name": "public_tax" }
```

Effects:

- Removes only the explicit public ABI binding.
- Internal ABI symbols remain stable.
- Object artifacts remain reusable; affected final link products must be
  relinked.

## Atomic Apply Batch

`codedb apply` accepts a single operation or a batch:

```json
{
  "schema": "codedb/apply/v1",
  "branch": "main",
  "expect_root_hash": "sha256:optional-root",
  "operations": [
    { "kind": "rename_symbol", "name": "tax", "new_name": "vat" },
    { "kind": "set_export", "name": "vat", "exported_name": "public_tax" }
  ]
}
```

If any operation conflicts or errors, the whole batch rolls back. The result is
canonical `codedb/apply-result/v1` JSON with:

```text
status
committed
old_root_hash
new_root_hash
history_hash
operation_count
processed_operation_count
applied_operation_count
results
```

Each result includes root hashes, migration hash, history hash, semantic impact,
type-check status, and structured build impact.

See [APPLY.md](APPLY.md) for the full JSON shape.

## Expressions in Apply JSON

Function bodies use structural `RawExpr` JSON:

```json
{ "kind": "literal_i64", "value": "100" }
{ "kind": "literal_bool", "value": true }
{ "kind": "unit" }
{ "kind": "param_name", "name": "subtotal" }
{ "kind": "param_ref", "index": 0 }
{ "kind": "call", "name": "tax", "args": [] }
{ "kind": "binary", "op": "+", "left": {}, "right": {} }
{ "kind": "unary", "op": "!", "expr": {} }
{ "kind": "let", "name": "x", "type": "i64", "value": {}, "body": {} }
{ "kind": "if", "cond": {}, "then": {}, "else": {} }
```

Text CLI expressions use projection syntax, for example:

```text
subtotal * 18 / 100
let x: i64 = subtotal + 1 in x * x
if flag then 1 else 0
!flag
-amount
()
```

## Diff and Build Impact

Capture roots before and after a migration:

```bash
REPLACE_OUT=$(cargo run -- replace-body "$DB" vat "subtotal * 19 / 100")
OLD_ROOT=$(printf '%s\n' "$REPLACE_OUT" | awk '/^old_root / { print $2 }')
NEW_ROOT=$(printf '%s\n' "$REPLACE_OUT" | awk '/^new_root / { print $2 }')
cargo run -- diff "$DB" "$OLD_ROOT" "$NEW_ROOT"
cargo run -- diff "$DB" "$OLD_ROOT" "$NEW_ROOT" --json
```

Build impact kinds include:

```text
metadata_only
relink_only
recompile_symbols
recompile_dependents
full_rebuild
```

## Native Build Walkthrough

Native compilation starts from typed/lowered semantic objects, not from C
source.

```bash
NATIVE_DB=native-demo.codedb.sqlite
rm -f "$NATIVE_DB" tax.o total.o main.o main.link.json native-demo
cargo run -- init "$NATIVE_DB"
cargo run -- import "$NATIVE_DB" examples/shop.cdb

cargo run -- emit-ir "$NATIVE_DB" main --out main.ir.json
cargo run -- emit-object "$NATIVE_DB" tax --target x86_64-unknown-linux-gnu --out tax.o
cargo run -- emit-object "$NATIVE_DB" total --target x86_64-unknown-linux-gnu --out total.o
cargo run -- emit-object "$NATIVE_DB" main --target x86_64-unknown-linux-gnu --out main.o
cargo run -- link-native "$NATIVE_DB" main --target x86_64-unknown-linux-gnu --out main.link.json
cargo run -- build-plan "$NATIVE_DB" main --target x86_64-unknown-linux-gnu --json
```

Use `build` when the requested target is linkable by the host:

```bash
cargo run -- build "$NATIVE_DB" main --out native-demo
./native-demo
echo $?
```

Renaming `tax` to `vat` does not change any native object bytes. Replacing the
body of `tax` changes only that function's object; callers can keep their object
artifacts because calls use stable ABI symbols.

## History Export and Import

Exported history rebuilds a database without copying SQLite:

```bash
HISTORY_DB=history-demo.codedb.sqlite
REPLAY_DB=history-replay.codedb.sqlite
rm -f "$HISTORY_DB" "$REPLAY_DB" history.ndjson

cargo run -- init "$HISTORY_DB"
cargo run -- import "$HISTORY_DB" examples/shop.cdb
cargo run -- rename "$HISTORY_DB" tax vat
cargo run -- export-history "$HISTORY_DB" --branch main --out history.ndjson

cargo run -- init "$REPLAY_DB"
cargo run -- import-history "$REPLAY_DB" history.ndjson
cargo run -- branches "$REPLAY_DB" --json
cargo run -- verify "$REPLAY_DB"
```

Native artifacts are disposable after import and can be regenerated from the
rebuilt root plus target options.
