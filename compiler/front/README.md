# compiler/front - Front-End to Lowered IR (Ladder Rung A)

Status: in progress (docs/PLAN_V3.md Phase 15; milestone V3.4). The self-hosted
front-end is built importer-first, smallest-corpus-first: each `.cdb` here computes,
in CodeDB's own language, a content address the Rust implementation also computes,
and is gated by byte-equality against a Rust oracle (`tests/selfhost_frontend.rs`).

Landed so far:

| File | What it self-hosts | Oracle |
| --- | --- | --- |
| `lex.cdb` | the lexer: source bytes to the token-stream probe `tokens <n> fnv32 <digest>` | `codedb::token_probe`, byte-equal on the whole committed corpus |
| `sha256.cdb` | general multi-block SHA-256 of stdin to hex; `obj_hash` entry frames `OBJECT_DOMAIN \|\| kind \|\| schema \|\| payload` | `codedb::sha256_hex` and real `emit-objects` object hashes |
| `lib.cdb` | shared SHA-256 core, object-hash framing, hex + stdin plumbing (imported first) | (composed into the others) |
| `import.cdb` | the importer: source to the ProgramRoot hash CodeDB would assign it | `codedb import` to `root`, byte-equal |
| `json.cdb` | the canonical JSON writer (front-end spine): measure/emit for the JSON value leaves (serde-faithful string escaping, bare integers, bools, null) plus the append substrate (`push_lit`/`push_byte`/`push_str`/`push_range`) and literal-skeleton object framing with exact measure for `{"k":v,...}` — array element splices to follow | `serde_json::to_string` / canonical object form, byte-equal |
| `object.cdb` | per-object-kind builders on `json.cdb` + `lib.cdb`'s SHA-256: Type (9 scalar kinds), SymbolBirth (3-key function/type + 4-key owned record_field/enum_variant), FunctionSignature (the first array — `params` of Type hashes), FunctionDef, and Expression (literal_i64 / literal_bool / binary, recursively composing child hashes) so far — exact-measured payloads and an exactly-sized `hash_object` preimage; more kinds + importer integration to follow | real `emit-objects` hashes, byte-equal |

`import.cdb` is the largest piece and reproduces the Rust importer's root hash across
a wide surface:

- **Expressions** (single function): the full i64 operator set (arithmetic, bitwise,
  shifts), comparisons and logical operators, `!` / unary `-` / `~`, bool literals,
  `if`, `let` plus identifier references (local_ref by de-Bruijn depth, param_ref by
  index), hex literals, sized integers (u8..u64 / i8..i64) with top-down expectation
  propagation, and `to_*` cast builtins.
- **Multi-function programs**: `create_function` migration/history chains for n
  functions (n <= 8), both independent and call-dependent (Kahn toposort, callee
  created before caller), with cross-symbol calls, call arguments, and parameters used
  across functions.
- **Recursion groups**: self-recursion and mutual recursion as `create_recursion_group`
  objects, for any single n-member strongly-connected clique (n = 1..8), with canonical
  member ordering reproduced from the Rust `canonical_clique_order` — 1-WL colour
  refinement for discretizing cliques, and the `clique_label_search`
  individualization-refinement for symmetric (automorphic) ones.
- **Type definitions**: single and N record/enum definitions via the `create_type`
  migration/history chain (type-only roots).

The importer threads the move-only source buffer by value, builds typed Expression
objects bottom-up (each parse function returns the object's content hash), and emits a
parallel raw-AST serialization for the migration `operation.body` (the dual
serialization of SPEC_V3 design principle 16). Canonical item ordering, object
construction, the migration/history hash chain, and SHA-256 are all computed in `.cdb`.

The content-addressing keystone (SPEC_V3 §5's rung-A prerequisite) is fully
self-hosted: general SHA-256 and object-domain framing live in `sha256.cdb` / `lib.cdb`,
so the language computes the same object and program identities CodeDB does. All
committed source passes the §11 checked-view gate. The only assumption is ASCII outside
string/comment content (every committed source satisfies it).

## Next: the front-end spine (Draft 1.1, Phase 15a.5)

The importer prototype proved the V3 thesis, but it pays too much for hand-coded
canonical JSON, duplicate raw/typed parsers, ad hoc graph ordering, fixed buffer
guesses, and backend-shaped source workarounds — the exact bug classes that bit the
project repeatedly (EnumDef key order, buffer-sizing traps, frame overflows, move-only
double-moves). Before resuming object-kind breadth, Phase 15a.5 turns the prototype
into reusable compiler infrastructure (see docs/PLAN_V3.md "Draft 1.1 continuation" and
SPEC_V3 §5A / §13A / design principles 14-19):

- `json.cdb` — canonical writer (measure / emit / escape, explicit key layout); the
  JSON value leaves (string escaping + bare int / bool / null) and literal-skeleton
  object framing (`push_lit` + exact measure) have landed; array element splices and
  the per-kind builders (object.cdb) + importer integration come next
- `object.cdb` — per-object-kind builders; Type (9 scalar kinds), SymbolBirth (3-key
  function/type + 4-key owned record_field/enum_variant), FunctionSignature (the first
  array — `params` of Type hashes), FunctionDef, Expression (literal_i64 / literal_bool /
  binary so far — recursively composing child hashes), and the content-addressing
  `hash_object` (exactly-sized preimage) — all content-hash byte-equal to `emit-objects` —
  have landed, then the rest of Expression (unary / if / let / refs / call / cast),
  record/enum/type defs, RecursionGroup, ProgramRoot, Migration, History
- `ast.cdb` — shared parsed item table / AST, one representation behind both serializers
- `graph.cdb` — Kahn ordering, SCC detection, canonical recursion-group ordering
- `migration.cdb` — birth seeds, pre/post templates, migration + history hashing
- buffered source ingestion; and Rust-side backend health: a first-class `frame-report`
  diagnostic and backend-bug fixtures for the v0 hazards

Then object-kind breadth resumes (15a.6+: mixed type/function roots, named-type
references, record construction, `case`, externs, mutual type cliques), and finally the
back half of rung A (typecheck through lowering, for IR-hash equality).

## The mixed compiler

The front half of the compiler as CodeDB objects, meeting the Rust native backend at
the lowered-IR seam. Sub-stages, each oracle-checked at its own artifact:

| Sub-stage | Output | Oracle |
| --- | --- | --- |
| importer | semantic objects | object-hash equality |
| type check | typed expressions | typed-object equality |
| borrow/effect/move/drop | accept/reject + diagnostics | same verdict |
| layout | layout JSON | layout-JSON equality |
| lowering | lowered IR | IR-hash equality |

- Depends on: Phases 6, 7, 9-14 (recursion, patterns, the codec stack, early exit,
  loops, strings, array fill, generics).
- The importer computes content hashes, so it cannot self-host until SHA-256 (Phase 9).
