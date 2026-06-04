# Structural Apply JSON

`codedb apply <db> --json <file>` applies a `codedb/apply/v1` document to the
selected semantic branch. The document `branch` field defaults to `main`.

Apply is atomic. If any operation errors or returns `conflict`, the whole batch
is rolled back and the selected branch, history rows, materialized indexes, and caches
remain unchanged. Per-operation errors return canonical `codedb/apply-result/v1`
JSON with `status: "error"` and `committed: false`.

## Document

```json
{
  "schema": "codedb/apply/v1",
  "branch": "main",
  "expect_root_hash": "sha256:optional-root",
  "operations": []
}
```

`branch` defaults to `main`. `expect_root_hash` may also be written as
`expect_root`. A batch-level expectation applies to the first operation. Each
operation may also set its own `expect_root_hash`.

A single operation may also be provided as the whole file. In that shorthand,
`schema` may be omitted, but if it is present it must be `codedb/apply/v1`.
Unknown fields in documents, operations, parameter specs, or expression bodies
are rejected.

## Operations

```json
{
  "kind": "create_function",
  "module": "main",
  "name": "tax",
  "birth_seed": "stable-agent-seed",
  "params": [{ "name": "subtotal", "type": "i64" }],
  "return_type": "i64",
  "effects": ["pure"],
  "body": { "kind": "literal_i64", "value": "1" }
}
```

```json
{ "kind": "rename_symbol", "name": "tax", "new_name": "vat" }
```

```json
{ "kind": "move_symbol", "name": "tax", "new_module": "billing" }
```

```json
{
  "kind": "replace_function_body",
  "name": "tax",
  "body": { "kind": "literal_i64", "value": "2" }
}
```

```json
{
  "kind": "change_function_signature",
  "name": "tax",
  "params": [{ "name": "subtotal", "type": "i64" }],
  "return_type": "i64",
  "effects": ["io"]
}
```

```json
{
  "kind": "add_parameter",
  "name": "tax",
  "param": { "name": "rate", "type": "i64" },
  "default": { "kind": "literal_i64", "value": "20" }
}
```

```json
{ "kind": "delete_symbol", "name": "unused", "force": false }
```

```json
{ "kind": "create_alias", "name": "tax", "alias": "sales_tax" }
```

```json
{ "kind": "remove_alias", "name": "tax", "alias": "sales_tax" }
```

```json
{ "kind": "set_export", "name": "tax", "exported_name": "public_tax" }
```

```json
{ "kind": "remove_export", "name": "tax", "exported_name": "public_tax" }
```

```json
{
  "kind": "create_test",
  "name": "main_returns_120",
  "entry": "main",
  "expected": { "kind": "i64", "value": "120" },
  "category": "behavior"
}
```

```json
{ "kind": "delete_test", "name": "main_returns_120" }
```

For non-create operations, `module` defaults to `main`. `symbol` may be supplied
to bind directly to stable identity; otherwise CodeDB resolves `name` in the
expected root. `move_symbol` changes the module metadata for the symbol's names
without changing `symbol_hash`, function definitions, signatures, or native
object cache keys.

`add_parameter` extends the target function signature and, when `default` is
provided, appends that argument at direct call sites in the same atomic
migration. If live call sites exist, `default` is required.

`create_test` categories are `behavior` (default), `projection`, and `export`.
Incremental test impact uses the category to decide whether rename/export-only
changes should select a test.

## Types

Function signatures, parameters, let bindings, enum constructors, and structural
JSON operations accept these type strings:

```text
i64
bool
unit
record {amount: i64, tax: i64}
enum {none: unit, some: i64}
```

Record fields and enum variants are projection-safe identifiers. Record and enum
type objects are structural and content-addressed; they do not imply heap
allocation or a managed runtime.

## Effects

Function signatures may declare effects:

```text
fn total(subtotal: i64) -> i64 = subtotal
fn read_counter() -> i64 effects[io] = 41
```

Structural `create_function` and `change_function_signature` operations accept
an optional `effects` array. Omitted effects and `["pure"]` are normalized to the
default pure signature. Non-pure effects are part of the function signature hash.

Initial effects:

```text
pure
trap
io
state
alloc
ffi
concurrent
```

`pure` cannot be combined with other effects. The current scaffold validates
call propagation: a function that calls an `io`, `ffi`, or otherwise effectful
function must declare those effects itself. Built-in arithmetic is still treated
as pure in this phase.

## External Functions

External functions are explicit semantic declarations. They have a symbol,
signature, ABI tag, link name, and optional library metadata, but no CodeDB body.

Projection syntax:

```text
extern fn host_value() -> i64 abi[c] effects[io, ffi] link_name "host_value" library "c"
```

Structural apply operation:

```json
{
  "kind": "create_external_function",
  "name": "host_value",
  "birth_seed": "ffi-host-value",
  "params": [],
  "return_type": "i64",
  "effects": ["io", "ffi"],
  "abi": "c",
  "link_name": "host_value",
  "library": "c"
}
```

Calls to externs type-check like normal calls. Their effects must be declared by
callers, and native link plans include them under `external_symbols` instead of
emitting CodeDB object files for them.

## Expressions

Bodies use structural `RawExpr` JSON:

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
{ "kind": "record", "fields": [{ "name": "amount", "value": {} }] }
{ "kind": "field_access", "target": {}, "field": "amount" }
{ "kind": "enum_construct", "type": "enum {none: unit, some: i64}", "variant": "some", "value": {} }
{
  "kind": "case",
  "expr": {},
  "arms": [
    { "variant": "none", "body": {} },
    { "variant": "some", "binding": "x", "body": {} }
  ]
}
```

Projection syntax supports the same surface:

```text
{amount: 100, tax: 20}
order.amount
enum {none: unit, some: i64}::some(41)
case maybe_value() of none => 0 | some(x) => x + 1
```

See [examples/shop.apply.json](../examples/shop.apply.json) for a complete
program built without projection text.

For operation-by-operation migration examples, see
[MIGRATIONS.md](MIGRATIONS.md).
