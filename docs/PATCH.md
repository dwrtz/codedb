# Semantic Patches

`codedb patch preview <db> --json <file>` previews a
`codedb/semantic-patch/v1` document. Preview matches semantic DAG structure,
lowers the patch to ordinary `codedb/apply/v1` structural operations, runs the
existing rollback-only apply preview, and leaves branch pointers, migrations,
objects, indexes, and caches unchanged.

`codedb patch apply <db> --json <file>` applies the same document as ordinary
structural migrations. Apply requires `expected_root`, commits atomically
through `codedb/apply/v1`, and records patch provenance in migration
`agent_json`.

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
`expect_root`, `expected_root_hash`, or `expect_root_hash`. If omitted, preview
matches the current branch root. Apply rejects documents without an expected
root so retries and stale-root conflicts stay explicit.

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

Supported replacements are `literal_i64`, `literal_bool`, `unit`, `call`,
`rename_symbol`, `extract_function`, `inline_function`, `add_parameter`,
`remove_unused_symbol`, `set_export`, and `remove_export`.

Extract a matched expression into a new function and replace the expression
with a call:

```json
{
  "schema": "codedb/semantic-patch/v1",
  "match": {
    "kind": "literal_i64",
    "value": "20",
    "within_name": "tax"
  },
  "replace": {
    "kind": "extract_function",
    "name": "rate"
  }
}
```

`extract_function` accepts optional `birth_seed`, `params`, `return_type`, and
call `args` fields. If `return_type` is omitted, CodeDB uses the matched
expression type.

Inline matched calls:

```json
{
  "schema": "codedb/semantic-patch/v1",
  "match": {
    "kind": "call",
    "target_name": "tax",
    "within_name": "total"
  },
  "replace": {
    "kind": "inline_function"
  }
}
```

Add a parameter to a matched function:

```json
{
  "schema": "codedb/semantic-patch/v1",
  "match": {
    "kind": "symbol",
    "name": "unused"
  },
  "replace": {
    "kind": "add_parameter",
    "name": "scale",
    "type": "i64",
    "default": { "kind": "literal_i64", "value": "1" }
  }
}
```

When live call sites exist, `default` is required. The patch applies the
signature extension and appends the default argument at direct call sites in one
semantic migration so the committed root remains type-valid.

Remove a symbol only when it has no live references or semantic tests:

```json
{
  "schema": "codedb/semantic-patch/v1",
  "match": {
    "kind": "symbol",
    "name": "unused"
  },
  "replace": {
    "kind": "remove_unused_symbol"
  }
}
```

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

Apply returns `codedb/semantic-patch-apply-result/v1` JSON with the same match
and planning fields plus:

```text
committed
old_root_hash
new_root_hash
old_history_hash
new_history_hash
semantic_summary
apply_result
patch_hash
```

`apply_result` is the committed `codedb/apply-result/v1` report. Each committed
migration stores `agent_json.semantic_patch` with the patch hash, match summary,
replacement, planned operation kinds, branch, and expected root.
