# compiler/front - Front-End to Lowered IR (Ladder Rung A)

Status: in progress (docs/PLAN_V3.md Phase 15; milestone V3.4). Sub-stage 15a.1
is landed: `lex.cdb` is the self-hosted lexer. It reads source bytes from stdin
and prints the token-stream probe `tokens <count> fnv32 <digest>`, byte-equal to
the Rust reference `codedb::token_probe` (gate: `tests/selfhost_frontend.rs`) on a
varied ASCII corpus and on real committed string-free sources, and the committed
source passes the §11 checked-view gate. The 15a.0 oracle substrate
(`emit-objects` for the importer's object/root-hash equality; `emit-tokens` for
the lexer) is in place. This stage's corpus is ASCII and free of string /
byte-string literals (a token's text is then a direct source slice, matching the
Rust `char` walk); string-literal lexing, the parser (15a.2), and the object
builder → root-hash oracle (15a.3) are the next increments.

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
