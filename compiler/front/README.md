# compiler/front - Front-End to Lowered IR (Ladder Rung A)

Status: in progress (docs/PLAN_V3.md Phase 15; milestone V3.4). Sub-stage 15a.1
is landed: `lex.cdb` is the self-hosted lexer. It reads source bytes from stdin
and prints the token-stream probe `tokens <count> fnv32 <digest>`, byte-equal to
the Rust reference `codedb::token_probe` (gate: `tests/selfhost_frontend.rs`) on a
varied corpus AND on the entire committed corpus token-for-token — all of `std/*`,
`examples/v3/*`, the 1700-line `compiler/eval/eval.cdb`, and the lexer itself —
including `"`/`b"` string and byte-string literals (folded over their decoded
bytes). The committed source passes the §11 checked-view gate. The 15a.0 oracle
substrate (`emit-objects` for the importer's object/root-hash equality;
`emit-tokens` for the lexer) is in place. The only assumption is ASCII outside
string/comment content (every committed source satisfies it).

The content-addressing keystone is also landed: `sha256.cdb` is general
multi-block SHA-256 of arbitrary stdin bytes → hex, byte-equal to
`codedb::sha256_hex` across empty input, every padding edge, multi-block messages,
and all 256 byte values. SPEC_V3 §5 makes this the rung-A prerequisite (the
importer cannot self-host until the language computes SHA-256); the example
`examples/v3/sha256.cdb` proved only the fixed "abc" block, so this generalizes it
(ingestion, padding, multi-block chaining, hex).

The self-hosted parser has started landing too (15a.2). `parse.cdb` reads source
from stdin, parses it with recursive descent (lex-on-demand: `lex1` returns the
next token's geometry without a separate token buffer), and prints the AST-shape
probe `items <count> ast32 <digest>`, byte-equal to the Rust reference
`codedb::ast_probe` (`emit-ast <file>`). The probe folds an FNV-1a-32 over a
STREAMING traversal of the AST — keyword-led forms pre-order, infix `Binary`
post-order via precedence climbing, lists sentinel-encoded, blobs
length-prefixed — so a streaming parser reproduces it without buffering. This
increment covers the EXPRESSION CORE over scalar pure functions: i64/bool
literals, parameter names, calls, the full operator set with precedence (incl.
`<<`/`>>` as two `<`/`>` tokens), prefix unary, parentheses/unit, and
`let`/`if`/`return`, plus multi-item programs and comments. Staged follow-on:
strings/records/arrays/enums/field-index/fold-loop-case/borrows, the type
normalizer (non-scalar types), modules, generics, and effects — then the object
builder (15a.3).

The object-hash wrapper is landed on top of it: the `obj_hash` entry of
`sha256.cdb` reads `kind\nschema\npayload` and frames `OBJECT_DOMAIN || kind || \0
|| schema || \0 || payload` before hashing, reproducing
`src/store.rs::hash_object_canonical` (`sha256:`+hex) exactly. Its oracle is
`emit-objects` itself — every dump line is a real `(kind, schema, payload → hash)`
case — so the `.cdb` provably computes the same object hashes CodeDB does. The
content-addressing core now fully self-hosts. Remaining increments: the object
builder (parsed items → canonical-JSON payloads) + birth identity → root-hash
oracle (15a.3), and the parser (15a.2, tokens → AST).

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
