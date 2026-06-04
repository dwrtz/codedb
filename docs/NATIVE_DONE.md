# NATIVE_DONE.md - V2 Native-Done Checklist

Status: Phase 1 policy

This checklist defines the v2 completion gate for semantic language and
program-model features. A v2 feature is not done when it only works in
projection text, structural import, or the reference evaluator. It is done only
when native artifacts prove the same semantic behavior.

## Completion Rule

Every v2 feature gate is native-required.

For a feature to pass the gate, the implementation must satisfy each applicable
layer:

- semantic object payloads exist;
- canonical hashing is deterministic;
- object edges are indexed;
- root and branch integration preserves the feature;
- projection syntax exists where the feature is user visible;
- structural apply JSON exists where the feature is mutable by operation;
- semantic patch support exists where applicable;
- type checking succeeds and fails deterministically;
- region, borrow, move, and drop checking exists where applicable;
- effect checking exists where applicable;
- reference evaluator behavior exists where practical as an oracle;
- semantic traces and debug events use stable semantic identities;
- lowered IR represents the feature without opaque host execution;
- native object backend compiles the feature;
- ABI and layout rules are deterministic for the target;
- link or executable flow works where the feature reaches an entry point;
- artifact cache keys include all target and layout inputs;
- native-required semantic tests pass;
- replay, export, and import preserve the feature;
- verify rejects malformed or unsupported feature state.

## Forbidden Completion Paths

The following do not satisfy a v2 feature gate:

- evaluator-only execution;
- projection-only support;
- lowering to a hidden semantic runtime dispatcher;
- treating unsupported native codegen as a skipped success;
- opaque host calls that execute CodeDB semantic objects;
- claiming completion without a native-required acceptance test.

## Native-Required Test Policy

V2 acceptance tests must be marked native-required once the test-case schema
supports that flag. Until Phase 2 adds the executable test-harness field, v2
plans and fixtures should label these gates as native-required in documentation.

The intended test-case shape is:

```json
{
  "schema": "codedb/test-case/v2",
  "mode": "reference_and_native",
  "native_required": true
}
```

In native-required mode, unsupported native backend behavior is a failure, not a
skip. The reference evaluator may compare behavior as an oracle, but it is not
the acceptance backend.

## Phase 1 Feature Gates

Phase 1 opens only the version-boundary documentation gate. It is accepted when:

- `docs/SPEC_V2.md` and `docs/PLAN_V2.md` define v2 as the native semantic
  programming track;
- README links the v0, v1, and v2 documentation tracks;
- `examples/v2/README.md` names the required v2 acceptance programs;
- v2 docs explicitly forbid interpreter fallback for feature completion;
- this checklist marks future v2 feature gates as native-required;
- existing tests still pass;
- no command behavior changes are required.
