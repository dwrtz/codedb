# Artifact and Object Model

This document describes the object payloads and derived artifacts supported by
the current prototype. The source of truth is still the semantic object DAG plus
migration history. Projections, lowered IR, native objects, link plans, and
executables are disposable artifacts.

## Pipeline

```text
ProgramRoot
  -> FunctionDef(symbol_hash, function_sig_hash, typed_body_expr_hash)
  -> lowered_ir cache artifact
  -> object_file cache artifact
  -> LinkPlanInput object
  -> link_plan cache artifact
  -> executable cache artifact
```

The C output is separate:

```text
ProgramRoot -> canonical source projection
ProgramRoot -> c_projection debug artifact
```

`c_projection` is not the primary native backend.

## Object Store Rules

Every object row has:

```text
hash
kind
schema_version
payload_json
payload_size_bytes
```

Hashes use canonical JSON and the object domain:

```text
sha256("codedb/object/v1\0" || kind || "\0" || schema_version || "\0" || canonical_payload)
```

Any string in a payload that starts with `sha256:` is treated as a potential
object reference. Existing referenced objects are materialized into
`object_edges`.

## Current Object Kinds

### Type

Built-in types are content-addressed objects:

```json
{ "type_kind": "I64" }
{ "type_kind": "Bool" }
{ "type_kind": "Unit" }
```

Projection names are `i64`, `bool`, and `unit`.

### SymbolBirth

Stable symbol identity is born once:

```json
{
  "symbol_kind": "function",
  "birth_history_hash": "genesis",
  "local_nonce": "import:main:tax:0"
}
```

`birth_history_hash` is either `genesis` or a history hash. `local_nonce` comes
from import or from an apply document `birth_seed`.

### FunctionSignature

```json
{
  "params": ["sha256:type-hash"],
  "return": "sha256:type-hash",
  "abi": "codedb-v0-internal",
  "effects": []
}
```

The current ABI tag is `codedb-v0-internal`.

### Expression

All expressions include `expr_kind` and `type`.

```json
{ "expr_kind": "literal_i64", "value": "100", "type": "sha256:i64-type" }
{ "expr_kind": "literal_bool", "value": true, "type": "sha256:bool-type" }
{ "expr_kind": "literal_unit", "type": "sha256:unit-type" }
{ "expr_kind": "param_ref", "index": 0, "type": "sha256:type" }
{ "expr_kind": "local_ref", "depth": 0, "type": "sha256:type" }
```

Calls use stable symbol hashes, not display names:

```json
{
  "expr_kind": "call",
  "symbol": "sha256:symbol",
  "args": ["sha256:arg-expr"],
  "type": "sha256:return-type"
}
```

Binary operations support `+`, `-`, `*`, `/`, `==`, `!=`, `<`, `<=`, `>`, `>=`,
`&&`, and `||`:

```json
{
  "expr_kind": "binary",
  "op": "+",
  "left": "sha256:left-expr",
  "right": "sha256:right-expr",
  "type": "sha256:type"
}
```

Unary operations support integer negation and boolean not:

```json
{
  "expr_kind": "unary",
  "op": "!",
  "expr": "sha256:expr",
  "type": "sha256:bool-type"
}
```

Let bindings store the typed value and body. References inside the body become
`local_ref` expressions by lexical depth, so repeated references can share one
expression hash.

```json
{
  "expr_kind": "let",
  "binding_name": "x",
  "binding_type": "sha256:type",
  "value": "sha256:value-expr",
  "body": "sha256:body-expr",
  "type": "sha256:body-type"
}
```

Conditionals require a boolean condition and equal branch types:

```json
{
  "expr_kind": "if",
  "cond": "sha256:cond-expr",
  "then": "sha256:then-expr",
  "else": "sha256:else-expr",
  "type": "sha256:branch-type"
}
```

### FunctionDef

```json
{
  "symbol": "sha256:symbol-birth",
  "function_sig_hash": "sha256:function-signature",
  "typed_body_expr_hash": "sha256:expression"
}
```

Display names and parameter names are not in `FunctionDef`.

### FunctionInterface

Indexing creates a small interface object used for cache identity:

```json
{
  "symbol_hash": "sha256:symbol-birth",
  "signature_hash": "sha256:function-signature",
  "internal_abi_symbol": "codedb_0123456789abcdef"
}
```

### ProgramRoot

`ProgramRoot` represents one complete program state:

```json
{
  "symbols": [
    {
      "symbol": "sha256:symbol-birth",
      "definition": "sha256:function-def",
      "signature": "sha256:function-signature"
    }
  ],
  "names": [
    {
      "module": "main",
      "display_name": "tax",
      "symbol": "sha256:symbol-birth",
      "is_preferred": true
    }
  ],
  "param_names": [
    {
      "symbol": "sha256:symbol-birth",
      "names": ["subtotal"]
    }
  ],
  "exports": [
    {
      "symbol": "sha256:symbol-birth",
      "exported_name": "public_tax"
    }
  ],
  "metadata": {}
}
```

Root payloads are normalized before hashing. Symbol entries sort by symbol
hash, names by module/display/symbol/preferred flag, parameter names by symbol,
and exports by exported name then symbol.

### LinkPlanInput

Link plan preparation stores a deterministic link input object:

```json
{
  "schema": "codedb/link-input/v1",
  "target_triple": "x86_64-unknown-linux-gnu",
  "entry_symbol_hash": "sha256:symbol",
  "entry_abi_symbol": "codedb_0123456789abcdef",
  "object_artifact_hashes": ["sha256:bytes"],
  "object_cache_keys": ["sha256:cache-key"],
  "export_map": [],
  "output_kind": "executable",
  "link_options": []
}
```

This object keys link-plan caching. It is derived from semantic roots and object
artifacts, not hand-authored source truth.

## Artifact Kinds

Current artifact kinds are typed values:

```text
canonical_source
c_projection
typed_expression
function_dependency_set
interface_hash
implementation_hash
lowered_ir
object_file
link_plan
executable
```

Projection artifacts:

```text
canonical_source
c_projection
```

Compiler artifacts:

```text
lowered_ir
object_file
link_plan
executable
```

Type-checking and planner artifacts:

```text
typed_expression
function_dependency_set
interface_hash
implementation_hash
```

## Cache Keys

`compile_cache.cache_key_json` stores a canonical `codedb/cache-key/v1` value:

```json
{
  "schema": "codedb/cache-key/v1",
  "artifact_kind": "object_file",
  "input_hash": "sha256:function-def",
  "dependency_interface_hashes": ["sha256:interface"],
  "dependency_implementation_hashes": [],
  "backend_id": "native-elf-x86_64-v0",
  "target_triple": "x86_64-unknown-linux-gnu",
  "abi_tag": "codedb-v0-internal",
  "relocation_model": "relocation:default",
  "code_model": "code-model:default",
  "optimization_level": "opt:none",
  "compiler_version": "codedb-0.1.0",
  "pipeline_version": "pipeline:v1",
  "runtime_sentinel": "runtime:none"
}
```

The cache key is a `codedb/cache/v1` hash over this canonical JSON. Target
triple, backend ID, ABI tag, compiler version, pipeline version, dependency
interfaces, dependency implementations, and runtime sentinel are all explicit.

## Cache Metadata Wrappers

Text artifacts use:

```json
{
  "schema": "codedb/artifact-metadata/v1",
  "artifact_kind": "c_projection",
  "input_hash": "sha256:root",
  "backend_id": "projection",
  "target_triple": "c_source",
  "content_kind": "text",
  "text": "/* deterministic source */",
  "text_hash": "sha256:bytes"
}
```

JSON artifacts use:

```json
{
  "schema": "codedb/artifact-metadata/v1",
  "artifact_kind": "lowered_ir",
  "input_hash": "sha256:function-def",
  "backend_id": "lowering-v1",
  "target_triple": "target-independent-memory-ir-v1",
  "content_kind": "json",
  "metadata": {},
  "metadata_hash": "sha256:bytes"
}
```

Byte artifacts use:

```json
{
  "schema": "codedb/artifact-metadata/v1",
  "artifact_kind": "object_file",
  "input_hash": "sha256:function-def",
  "backend_id": "native-elf-x86_64-v0",
  "target_triple": "x86_64-unknown-linux-gnu",
  "content_kind": "bytes",
  "metadata": {},
  "bytes_hash": "sha256:bytes"
}
```

`artifact_bytes` is required for `object_file` and `executable`.

## Lowered IR

Lowered functions use `codedb/lowered-function-ir/v2`:

```json
{
  "schema": "codedb/lowered-function-ir/v2",
  "symbol_hash": "sha256:symbol",
  "function_def_hash": "sha256:function-def",
  "function_sig_hash": "sha256:function-signature",
  "typed_body_expr_hash": "sha256:expression",
  "params": [{ "slot": 0, "type_hash": "sha256:type" }],
  "locals": [{ "slot": 0, "type_hash": "sha256:type" }],
  "return_type_hash": "sha256:type",
  "operations": [],
  "debug_map": {
    "schema": "codedb/lowered-debug-map/v1",
    "operations": [
      {
        "lowered_op_id": "op:v0",
        "value_id": "v0",
        "lowered_op_kind": "const_i64",
        "expr_hash": "sha256:expression"
      }
    ],
    "expr_to_ops": [
      {
        "expr_hash": "sha256:expression",
        "lowered_op_ids": ["op:v0"]
      }
    ]
  }
}
```

Operations include `param`, `const_i64`, `const_u8`, `const_bool`, `const_unit`,
`static_data_address`, `construct_slice`, `unary`, `binary`, `call`, `if`,
`fold`, `heap_alloc`, `deref_box`, `addr_of_param`, `addr_of_local`,
`addr_of_field`, `addr_of_index`, `load`, `store`, `copy`, `move`, `drop`,
`borrow_debug`, and `return`. Calls target `target_symbol_hash`; they do not
target display names.
`debug_map` records stable lowered operation IDs for value-producing operations,
including address and load/copy/move operations, and maps expression hashes back
to those IDs.

Inspect lowered IR:

```bash
cargo run -- emit-ir demo.codedb.sqlite main --out main.ir.json
```

## Native Object Artifacts

Current object backends:

```text
x86_64-unknown-linux-gnu   -> ELF64 relocatable object
aarch64-apple-darwin      -> Mach-O arm64 relocatable object
```

Metadata for native objects uses `codedb/native-object/v1`:

```json
{
  "schema": "codedb/native-object/v1",
  "backend_id": "native-elf-x86_64-v0",
  "object_format": "elf64-x86-64-relocatable",
  "target_triple": "x86_64-unknown-linux-gnu",
  "symbol_hash": "sha256:symbol",
  "function_def_hash": "sha256:function-def",
  "function_sig_hash": "sha256:function-signature",
  "typed_body_expr_hash": "sha256:expression",
  "lowered_ir_schema": "codedb/lowered-function-ir/v2",
  "defined_symbols": ["codedb_0123456789abcdef"],
  "called_symbols": ["sha256:callee-symbol"],
  "relocations": [
    {
      "offset": 42,
      "kind": "R_X86_64_PLT32",
      "target_symbol_hash": "sha256:callee-symbol",
      "target_abi_symbol": "codedb_fedcba9876543210"
    }
  ],
  "static_data": [
    {
      "static_data_hash": "sha256:static-data",
      "bytes_hex": "68656c6c6f",
      "section": ".rodata",
      "section_offset": 0,
      "offset": 320,
      "len": 5
    }
  ],
  "debug_metadata": {
    "schema": "codedb/native-debug-metadata/v1",
    "text_section": ".text",
    "text_size": 64,
    "ranges": [
      {
        "symbol_hash": "sha256:symbol",
        "function_def_hash": "sha256:function-def",
        "lowered_op_id": "op:v0",
        "value_id": "v0",
        "lowered_op_kind": "const_i64",
        "expr_hash": "sha256:expression",
        "text_offset_start": 12,
        "text_offset_end": 24
      }
    ]
  }
}
```

Apple Mach-O metadata also includes `object_symbols` with underscore-prefixed
Mach-O symbol names, branch relocations use `ARM64_RELOC_BRANCH26`, and static
data entries report section `__TEXT,__const` instead of `.rodata`.

Internal ABI symbols are derived only from symbol identity:

```text
codedb_<first 16 hex characters of symbol_hash>
```

Renames and aliases do not change native object identity or native debug
metadata. Body changes can change expression hashes, lowered operation ranges,
and object bytes.

## Link Plans and Executables

`link-native` emits and caches `codedb/link-plan/v1`:

```json
{
  "schema": "codedb/link-plan/v1",
  "input_hash": "sha256:link-plan-input",
  "target_triple": "x86_64-unknown-linux-gnu",
  "entry_symbol_hash": "sha256:symbol",
  "entry_abi_symbol": "codedb_0123456789abcdef",
  "objects": [
    {
      "symbol_hash": "sha256:symbol",
      "object_cache_key": "sha256:cache-key",
      "object_artifact_hash": "sha256:bytes",
      "static_data": [
        {
          "static_data_hash": "sha256:static-data",
          "bytes_hex": "68656c6c6f",
          "section": ".rodata",
          "section_offset": 0,
          "offset": 320,
          "len": 5
        }
      ],
      "debug_metadata": {
        "schema": "codedb/native-debug-metadata/v1",
        "text_section": ".text",
        "text_size": 64,
        "ranges": []
      }
    }
  ],
  "export_map": [],
  "external_symbols": [],
  "output_kind": "executable",
  "link_options": []
}
```

`build-plan --json` emits `codedb/native-build-plan/v1`, which is a command
inspection artifact showing the planned artifact jobs, link plan cache key,
reachable object list, export map, external symbols, platform capsule externs,
stdlib capability metadata, and link options. It does not compile artifacts;
`link_plan_hash` is `null` until the link plan is materialized by `link-native`
or `build`.

`build` invokes the host `cc` linker when the requested target can be linked by
the host. Executable cache entries use `codedb/executable/v1` metadata and store
the executable bytes in `artifact_bytes`.

## SQLite Architecture

Semantic source of truth:

- `objects`: immutable object payloads.
- `object_edges`: recomputed object-reference edges from object payloads.
- `migrations`: semantic operations with input root, output root, preconditions,
  postconditions, and agent metadata.
- `histories`: ordered migration-history heads.
- `branches`: named root/history pointers. V0 uses `main`.

Materialized indexes:

- `root_symbols`: root to symbol/definition/signature rows.
- `root_names`: module/display-name bindings and aliases.
- `root_exports`: explicit public ABI export map.
- `dependencies`: direct function call dependencies by symbol hash.
- `source_search`: FTS projection cache for inspection.

Disposable artifact cache:

- `compile_cache`: typed artifact rows keyed by canonical `CacheKeyInput`;
  stores text or JSON metadata in `artifact_json` and binary artifacts in
  `artifact_bytes`.

Verification recomputes object hashes, object edges, materialized indexes,
cache key hashes, artifact metadata hashes, native object byte hashes, native
debug metadata, link plans, and executable metadata where possible.

## Common Artifact Commands

```bash
cargo run -- export demo.codedb.sqlite --branch main --out projection.cdb
cargo run -- emit-c demo.codedb.sqlite main --out projection.c
cargo run -- emit-ir demo.codedb.sqlite main --out main.ir.json
cargo run -- emit-object demo.codedb.sqlite main --target x86_64-unknown-linux-gnu --out main.o
cargo run -- link-native demo.codedb.sqlite main --target x86_64-unknown-linux-gnu --out main.link.json
cargo run -- build-plan demo.codedb.sqlite main --target x86_64-unknown-linux-gnu --json
cargo run -- build demo.codedb.sqlite main --out demo-native
```

`build` returns an executable whose process exit code is the entry function
result for `i64` and `bool` entry functions.
