# compiler/eval - Reference Evaluator in CodeDB (Ladder Rung 0)

Status: substrate landed (docs/PLAN_V3.md Phase 8; milestone V3.2) — the CIR
input artifact and the `string_set` primitive are in place; the `.cdb`
evaluator itself is the next stage.

Re-expresses the reference evaluator — the lowered-IR walker, the Value model,
and per-op evaluation — as CodeDB objects. This rung is off the compilation path:
the evaluator is the oracle, not a compile stage. It is the Pillar-1 warm-up
because an IR walker forces recursion (R1) and pattern richness (R14).

- Depends on: Phase 6 (recursion), Phase 7 (pattern matching).
- Oracle: agrees with the Rust evaluator on the entire existing test corpus,
  yielding a three-way oracle (CodeDB-eval == Rust-eval == native).

## Design

The CodeDB-hosted evaluator walks the **lowered IR**, not typed expressions
(SPEC_V3 §5 "the smallest recursive IR-walker"). The lowered IR is fully
layout-resolved — explicit byte offsets, slot sizes, embedded type layouts,
desugared patterns, monomorphized generics, explicit drops and traps — so the
natural Value model is a byte machine over simulated memory (a `string` buffer
written with `string_set`, read with `string_get`), and no type checker, layout
engine, or pattern compiler needs re-expression at this rung.

The oracle is *result equality* against the Rust evaluator, so the rung-0
corpus is the Rust evaluator's domain: pure-compute and argv programs.
Extern-calling programs are excluded on both sides equally (the Rust evaluator
refuses externs; CIR refuses to encode them).

## CIR — the input artifact

`codedb emit-cir <db> <entry> --target <triple> --out <file>` serializes the
lowered-IR closure of an entry as flat bytes a `.cdb` program can decode with
byte reads (`src/cir.rs` holds the authoritative format doc and the stable
opcode table). Properties, pinned by `tests/cir_artifact.rs`:

- **Faithful**: every emission decodes its own bytes and fails unless the
  decoded `LoweredFunctionIr`s are structurally identical to the lowered
  originals — an emission that loses information cannot succeed.
- **Deterministic**: byte-identical across re-emission and across an
  independent database rebuilt from the same source (deterministic birth
  identities -> same root -> same lowered IR -> same CIR).
- **Pre-resolved**: call targets are function-table indices, type hashes are
  per-function type-table indices, value ids are dense indices; the consumer
  never parses hashes. The function table is sorted by symbol hash; the entry
  is named by index. Monomorphic generic instances appear as ordinary table
  entries; generic templates do not lower and do not appear.
- **Fail-closed**: external functions in the closure are rejected; truncated,
  corrupt, or trailing-byte files fail to decode.

Layout summary (all integers little-endian; full doc in `src/cir.rs`):

```text
"CDIR" magic | version u32
string pool | data pool        (count, byte-lens, concatenated bytes)
target str | entry_index u32 | function table (symbol, offset, len)
per function: header hashes | layout table | type table | value table
              | return/params/locals | op stream | debug map
```

Each op is one opcode byte (the append-only table in `src/cir.rs::opcode`,
0=param .. 55=early_return) followed by its fields in struct order; blocks are
count-prefixed and nested (`if`/`case`/`fold`/`loop` carry their sub-blocks
inline), so the walker recurses exactly the way the spec's "smallest recursive
IR-walker" suggests.

## Evaluator staging (next)

1. loader/decoder in `.cdb` (argv path -> byte reads -> pools/tables),
   simulated memory over `string_with_capacity`/`string_set`/`string_get`;
2. scalar core (consts, all binary/unary widths with wrap+trap semantics,
   `int_cast`, `if`, load/store/copy/move, call/return/early-return) — gated
   by the three-way operator-conformance corpus;
3. aggregates (records/enums/arrays: addr-of ops, sized load/store, enum tags,
   `case`, bounds checks, slices, static data, `fold`, `loop`);
4. heap (box ops with a bump allocator, vec/string ops with capacity traps,
   argv forwarding shifted past the CIR path argument);
5. the corpus harness (`tests/selfhost_eval.rs`): manifest-driven
   Rust-eval-vs-CodeDB-evaluator sweep, plus the §11 checked-view gate for
   `compiler/eval/*.cdb`.
