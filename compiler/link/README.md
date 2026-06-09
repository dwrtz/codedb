# compiler/link - Link Plan (Ladder Rung C)

Status: skeleton (docs/PLAN_V3.md Phase 18; milestone V3.5)

Link-plan construction (reachable externs, capabilities, ABI symbols) as CodeDB
objects, closing the ladder: CodeDB compiles itself end to end, checked against
the Rust compiler at every seam.

- Depends on: Phase 17 (rung B).
- Oracle: link-plan JSON equals the Rust linker driver's; the fully self-hosted
  pipeline reproduces the Rust compiler's binaries for the acceptance corpus.
