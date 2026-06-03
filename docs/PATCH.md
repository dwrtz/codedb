# Semantic Patch Preview

`codedb patch preview <db> --json <file>` previews a
`codedb/semantic-patch/v1` document. Preview matches semantic DAG structure,
lowers the patch to ordinary `codedb/apply/v1` structural operations, runs the
existing rollback-only apply preview, and leaves branch pointers, migrations,
objects, indexes, and caches unchanged.

## Document

```json
{
  "schema": "codedb/semantic-patch/v1",
  "branch": "main",
  "expected_root": "sha256:optional-root",
  "match": {},
  "replace": {}
}
```

`branch` defaults to `main`. `expected_root` may also be written as
`expect_root`, `expected_root_hash`, or `expect_root_hash`. If omitted, the
current branch root is matched and previewed.

## Expression Patches

Replace a literal inside a function body:

```json
{
  "schema": "codedb/semantic-patch/v1",
  "match": {
    "kind": "literal_i64",
    "value": "20",
    "within_name": "tax"
  },
  "replace": {
    "kind": "literal_i64",
    "value": "18"
  }
}
```

Retarget calls while preserving arguments:

```json
{
  "schema": "codedb/semantic-patch/v1",
  "match": {
    "kind": "call",
    "target_name": "tax",
    "within_name": "total"
  },
  "replace": {
    "kind": "call",
    "target_name": "fee",
    "args": "$same_args"
  }
}
```

Supported match kinds are `symbol`, `function_definition`, `expr`,
`literal_i64`, `literal_bool`, `call`, `type`, and `export`.

Supported preview replacements are `literal_i64`, `literal_bool`, `unit`,
`call`, `rename_symbol`, `set_export`, and `remove_export`.

## Result

Preview returns `codedb/semantic-patch-preview/v1` JSON with:

```text
matched_symbols
matched_expressions
matched_types
matched_exports
planned_operations
typecheck
build_impact
apply_preview
diagnostics
```

`apply_preview` is the nested rollback-only `codedb/apply-result/v1` report.
If a patch would fail type checking, preview returns `status: "error"` with a
type-check diagnostic and still leaves the branch unchanged.
