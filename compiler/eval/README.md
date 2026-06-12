# compiler/eval - Reference Evaluator in CodeDB (Ladder Rung 0)

Status: COMPLETE (docs/PLAN_V3.md Phase 8; milestone V3.2). The `.cdb`
evaluator runs natively and is result-equal to the Rust evaluator on the
operator-conformance sweep, the per-feature scalar/aggregate/heap fixtures,
and the qualifying example corpus — including the complete sha256 digest —
and the committed sources pass the §11 checked-view gate
(`tests/selfhost_eval.rs`).

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

CIR additionally carries **consumer columns** — derived metadata pre-classified
at encode time so the `.cdb` walker never interprets hash strings or operator
names:

- each type-table row carries `meta_kind u8` + `meta_size u64` (unit/bool/
  i8..u64/pointer/aggregate-by-value/aggregate-indirect and the byte size),
  derived from the well-known scalar type hashes and the layout rows;
- each `binary`/`unary` op carries `verb u8` + `width u8` (bytes) + `signed u8`,
  derived from the operator-registry kind string.

Like the call-target indices, these are *renamings* of facts already explicit
in the IR (type names, layout rows, registry kinds) — never new semantics —
and the decode half of the honesty gate recomputes each column and fails on
any mismatch, so a CIR file provably carries the same classification the
registry and layout engine would produce.

## Execution design (pinned)

**Input protocol.** `eval-bin < program.cir [program-args...]`: the CIR
arrives on stdin and every process argument belongs to the evaluated program
(its `arg_count`/`arg_len`/`arg_byte` see the host list 1:1). A path argument
was the original sketch, but the v0 backend gives every op's value id an
8-byte frame slot, so the `[0x0; N]` path/chunk buffers cost ~24N frame bytes
(fill stores plus their address/bounds ids) and any N >= ~150 busts the
4095-byte arm64 frame budget; stdin needs only a 1-byte bounce buffer read in
a loop (`read(0, ptr, 1)` per byte — at rung-0 corpus sizes the syscall cost
is noise). The shell is the only extern-touching part of the evaluator;
everything after "bytes are in memory" is pure compute.

**Memory.** One `string` is the machine memory: a fixed generous capacity
(`string_with_capacity`), grown to its watermark with `string_push`, then
random-accessed with `string_get`/`string_set`. Addresses are byte offsets
into this buffer. The map:

```text
[0, image_len)            the CIR file image, verbatim
[meta, stack)             loader-built per-function metadata (see below)
[stack, heap)             call frames: a bump-up stack, popped on return
[heap, capacity)          heap: bump-up allocations, never freed
```

A single address space means static-data addresses point directly into the
image's data pool and raw-pointer corpus ops need no tagging. Exceeding a
region or the capacity traps (fail-loud), never silently grows.

**Per-function metadata (load-time prepass).** The image's tables are
fixed-width and indexed in place (type rows, layout rows, param/local rows,
the function table); only frame offsets need computing. At load, one pass per
function writes into the meta region: section/op-stream/type-table/layout-table
base offsets, param/local/value counts, the frame size, and per-param /
per-local frame offsets (params first, then locals by their declared
`size_bytes`, then one 8-byte cell per value id, each region 8-aligned).
Stage 3 extends the prepass with fixed temp offsets for aggregate-result
value ids (loops re-execute ops, so temps are slots, not a bump).

**Value model.** Mirrors the native backend, not the Rust evaluator: one
8-byte cell per dense value id. Scalar cells hold the value in canonical
extended form (sign-extended for i8/i16/i32, zero-extended for unsigned
widths); pointer-like cells (box/ref/raw pointer) hold an address; aggregate
cells hold the address of the value's bytes (`by_indirect` ABI). `int_cast`
truncates to the target width and re-extends by the target's signedness.
Comparisons pick signed/unsigned forms from the consumer columns; arithmetic
wraps at its width; `div`/`mod` trap on a zero right operand per the op's
trap field.

**Calls.** A call bumps the stack by the callee's frame size, copies argument
cells into param slots (aggregates pass as addresses), recurses into the
callee's op stream, and pops. An aggregate return uses the caller-provided
`return_address`. `early_return` unwinds the current function only (its
drops are already explicit ops on the early-exit edge).

**Drops and the heap.** `heap_alloc` bumps; `drop` and `free_box_shell` are
validated no-ops. The oracle is *result equality* and the Rust evaluator's
domain excludes externs, so drop execution is unobservable in the rung-0
corpus; the bump heap makes "use after free" impossible by construction.
Capacity traps on vec/string ops are enforced exactly like the native
runtime's.

**Output protocol.** On success the evaluator prints `ok:<value>` — the entry
result rendered from its return-type consumer column (signed widths as signed
decimal, unsigned as unsigned decimal, bool as `0`/`1`, unit as `unit`) — and
exits 0. On a trap it prints `trap:<code>` (e.g. `trap:division_by_zero`,
`trap:bounds_check`, or `trap:unsupported_op` past the implemented frontier)
and exits 101; a non-CIR input exits 65 silently. (Stage 1's five-number
loader probe was superseded by execution — its walk correctness is now
re-proven by every executed program.)

**State threading.** The memory string is move-only: helpers take it and
return it inside a small record (`{ mem: string, val: i64 }`); scalar loop
state rides in Copy records; byte pumps either use the `arg_string` loop shape
(the accumulator IS the string, the index derived from `string_len`) or
chunk-bounded recursion. This verbosity is deliberate dogfood: rung 0 is the
first big program written the way agents will have to write the compiler.

**Corpus.** A CIR-encodable program is extern-free by construction; the
rung-0 corpus is every such entry with a scalar result: the generated
operator-conformance fixtures (`tests/oracle_conformance.rs`) extended
three-way, the pure examples (`fnv1a`, `sha256`, `tokenizer`, recursion /
pattern / string / fmt / argv fixtures), and targeted aggregate + heap
programs. Aggregate-result entries need a canonical value serialization and
stay out until something forces them.

## Evaluator staging

1. **(done — substrate)** `string_set` + the CIR artifact + consumer columns;
2. **(done — Stage 1)** loader in `.cdb` (stdin -> 1-byte bounce reads ->
   image in the memory string -> header/pool/function-table/section walk),
   gated by a five-line probe vs `emit-cir`'s summary on real examples
   (superseded by execution in Stage 2);
3. **(done — Stage 2)** scalar core: per-function metadata + frames, consts,
   params, every binary/unary operator width and signedness (semantics
   inherited by construction through the language's own sized operators),
   `int_cast`, `if`, scalar load/store/copy/move, borrows/derefs as address
   cells, bounds checks, call/return/early-return, div/mod trap parity, and
   the `ok:`/`trap:` protocol — gated by a generated three-way conformance
   sweep with a fixture per `codedb::operator_kinds()` entry plus scalar
   control-flow/recursion programs (`tests/selfhost_eval.rs`);
4. **(done — Stage 3)** aggregates: addr-of field/payload/index through the
   explicit place offsets, aggregate load/move/copy as address aliases with
   byte copies only at stores (backend parity), enum tags (8 bytes at offset
   0), `case` with the last arm as default, `fold`/`loop` drivers over
   accumulator locals (early returns propagate out of iteration bodies),
   slices, static data via a load-time data-pool offset index, range checks,
   and the call ABI's hidden return slot + indirect-param entry copies —
   gated by the tokenizer + sha256 digest examples and a per-feature
   aggregate fixture, all result-equal to the Rust evaluator;
5. **(done — Stage 4)** heap: `heap_alloc` bumps (the pointer cell lives at
   the stack/heap boundary; drops and shell frees are validated no-ops),
   `unbox` copies the payload out (by-value records are raw byte patterns in
   cells, mirroring the backend's `passes_indirect == false` path), and the
   vec/string buffers run over `{ptr, len, capacity}` headers described by
   the layout rows' buffer consumer columns, trapping at capacity exactly
   like the native runtime (a DOCUMENTED divergence from the growable
   Rust-eval string model, pinned by a test); argv forwards 1:1 — gated by
   box-recursion (cons list), vec/string/fmt, and argv-parity fixtures;
6. **(done — Stage 5)** the corpus harness (`tests/selfhost_eval.rs`): the
   example-corpus manifest sweep (booleans, discount, fnv1a, the tokenizer,
   and the COMPLETE sha256 digest — all eight words), plus the §11
   checked-view gate (import the committed sources -> export -> re-import:
   byte-stable canonical projection, fixpoint root).

Phase 8 / milestone V3.2 is COMPLETE per the corpus definition pinned above:
CodeDB-eval == Rust-eval on the operator-conformance sweep, the per-feature
scalar/aggregate/heap fixtures, and the qualifying example corpus, with the
native backend as the transitive third leg. Extending the manifest with
further fixtures is mechanical; aggregate-result entries await a canonical
value serialization if something forces them.
