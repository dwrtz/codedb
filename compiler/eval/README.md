# compiler/eval - Reference Evaluator in CodeDB (Ladder Rung 0)

Status: skeleton (docs/PLAN_V3.md Phase 8; milestone V3.2)

Re-expresses the reference evaluator — the lowered-IR walker, the Value model,
and per-op evaluation — as CodeDB objects. This rung is off the compilation path:
the evaluator is the oracle, not a compile stage. It is the Pillar-1 warm-up
because an IR walker forces recursion (R1) and pattern richness (R14).

- Depends on: Phase 6 (recursion), Phase 7 (pattern matching).
- Oracle: agrees with the Rust evaluator on the entire existing test corpus,
  yielding a three-way oracle (CodeDB-eval == Rust-eval == native).
