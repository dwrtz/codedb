# examples/v3 - Self-Hosting Forcing Programs

Status: v3 acceptance fixture index

V3 acceptance comes in two forms: the self-hosting ladder rungs (under
`compiler/`, each checked against the Rust compiler by a determinism oracle) and
the forcing-function programs below, which drive the expressiveness floor the
front-end needs. Like v2, these are native-required gates: the reference
evaluator is an oracle only, and evaluator-only success does not accept a
feature.

Forcing-function programs:

| Program | Drives | Native-required gate |
| --- | --- | --- |
| `tokenizer.cdb` | Early exit on malformed input (R7); bytes->int parsing (R6). | A byte-stream tokenizer rejects malformed input and exits its loop early; verify proves drop/borrow correctness across the early-exit edge. |
| `sha256.cdb` | Bitwise / sized / cast stack (R4/R5/R6). | Hashes a byte slice to a digest matching a reference; validates the content hashing the self-hosted importer (rung A) depends on. |

Each program should carry the v2 fixture set: source projection, structural apply
JSON where useful, native-required tests, trace/debug, verify, and
replay/export/import.

The self-hosting ladder rungs live under the top-level `compiler/` tree (see
`compiler/README.md`). New gaps found while writing these programs are promoted
into `docs/ROADMAP.md` as new `R<n>` items, per its workflow, then into a
`docs/PLAN_V3.md` phase with a native-required fixture.

Implemented fixtures are added as the language features each program needs
(docs/PLAN_V3.md phases) land.
