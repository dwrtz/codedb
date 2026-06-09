# compiler/backend - Native Object Emission (Ladder Rung B)

Status: skeleton (docs/PLAN_V3.md Phase 17; milestone V3.5)

Native object emission (lowered IR -> `.o`) as CodeDB objects: machine-code
encoders for x86_64 and arm64, relocations, and ELF / Mach-O object writers. The
large back-half rung; staged last but not optional, and never replaced by
lowering to C. Machine-code encoding is bit-level, so it leans hard on the
bitwise / sized-integer / cast stack (Phase 9).

- Depends on: Phase 9 (bitwise/sized/casts), Phase 15 (rung A).
- Oracle: emitted `.o` bytes are identical to the Rust emitter on both targets.
