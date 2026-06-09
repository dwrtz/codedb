# compiler/front - Front-End to Lowered IR (Ladder Rung A)

Status: skeleton (docs/PLAN_V3.md Phase 15; milestone V3.4)

The front half of the compiler as CodeDB objects, meeting the Rust native backend
at the lowered-IR seam (the mixed compiler). Sub-stages, each oracle-checked at
its own artifact:

| Sub-stage | Output | Oracle |
| --- | --- | --- |
| importer | semantic objects | object-hash equality |
| type check | typed expressions | typed-object equality |
| borrow/effect/move/drop | accept/reject + diagnostics | same verdict |
| layout | layout JSON | layout-JSON equality |
| lowering | lowered IR | IR-hash equality |

- Depends on: Phases 6, 7, 9-14 (recursion, patterns, the codec stack, early
  exit, loops, strings, array fill, generics).
- Note: the importer computes content hashes, so it cannot self-host until
  SHA-256 (Phase 9) lands.
