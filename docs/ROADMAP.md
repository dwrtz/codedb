# ROADMAP.md — CodeDB Language and Compiler Improvements Roadmap

Status: Draft 1.0
Scope: forward-looking language/compiler gaps discovered while writing real
programs against the v2 native surface. This is distinct from
[PLAN_V2.md](PLAN_V2.md), which tracks the implemented phase plan (Phases 1–24).

## Purpose

PLAN_V2 says a feature is "done" when it compiles from semantic objects to a
native artifact and passes native-required tests. This roadmap is the dual: it
records language/compiler **gaps** — things you cannot express, or can only
express awkwardly — found by sitting down and writing useful programs.

The intended workflow is:

```text
write a useful program against the current v2 surface
  -> record every place the language forced an awkward workaround or a "can't"
  -> file it here with: discovery context, impact, current workaround, direction
  -> promote accepted items into a PLAN_V2 phase with a native-required fixture
```

Each item keeps a stable ID (`R<n>`) so tests, commits, and notes can reference
it. An item is only "resolved" when, like any v2 feature, it has a
native-required acceptance fixture — not just evaluator support.

This doc tracks two series of planned items: the **R-series**, discovered by
writing programs (below), and the **D-series**, the v2 plan's deliberate
deferrals now tracked as planned work (see
[Deferred from the v2 plan](#deferred-from-the-v2-plan-planned-d-series)). The
striking part is how little they overlap — see that section's takeaway.

## Severity legend

```text
blocking — an entire useful program class cannot be written at all
major    — expressible only via a workaround that distorts program structure
minor    — small friction; a clean local workaround exists
```

## Summary

| ID | Gap | Severity | Status | Discovered by |
| --- | --- | --- | --- | --- |
| R1 | No recursive or mutually-recursive functions | major | proposed | `examples/calc_vm.cdb` (2026-06-09) |
| R2 | No remainder/modulo operator | minor | proposed | `examples/calc_vm.cdb` (2026-06-09) |
| R3 | No integer→string formatting in the stdlib | major | proposed | `examples/calc_vm.cdb` (2026-06-09) |
| R4 | No bitwise operators (`& \| ^ ~ << >>`) | major | proposed | `examples/fnv1a.cdb` (2026-06-09) |
| R5 | Only `i64`/`u8`: no fixed-width/unsigned types or wrapping arithmetic | major | proposed | `examples/fnv1a.cdb` (2026-06-09) |
| R6 | No numeric casts/conversions (e.g. `u8`→`i64`) | major | proposed | `examples/fnv1a.cdb` (2026-06-09) |
| R7 | No early exit (`return`/`break`/`continue`) | major | probed | operator probe (2026-06-09) |
| R8 | No unbounded loop (`while`/`loop`/server loop) | major | proposed | `examples/http_server.cdb` (2026-06-09) |
| R9 | No array fill/repeat initializer (`[x; N]`) | minor | proposed | `examples/http_server.cdb` (2026-06-09) |
| R10 | No floating-point numbers (`f32`/`f64`) | major | probed | roadmap review (2026-06-09) |
| R11 | No generics / type parameters (`Option<T>`, `Vec<T>`) | major | probed | roadmap review (2026-06-09) |
| R12 | No process arguments (argv) or environment | major | probed | roadmap review (2026-06-09) |
| R13 | No first-class functions / function values | major | probed | roadmap review (2026-06-09) |
| R14 | `case` matches only enum variants (no literals/`_`/guards) | minor | probed | roadmap review (2026-06-09) |
| R15 | `string` is opaque (no index/concat/compare/slice/build) | major | probed | roadmap review (2026-06-09) |

Severity note: R4, R5, and R6 compound. Together they make hashing, checksums,
and codecs (the program class `fnv1a.cdb` represents) either impossible or
expressible only through arithmetic emulation that no one would ship. R8 is the
single biggest gap for *services*: without an unbounded loop, no server, daemon,
event loop, or REPL can be written — `http_server.cdb` works only by serving a
fixed number of connections and then exiting.

The **D-series** is the v2 plan's deliberate non-goals, now tracked as planned
items (full entries in [Deferred from the v2 plan](#deferred-from-the-v2-plan-planned-d-series)):

| ID | Capability | Severity | Status | Relates to |
| --- | --- | --- | --- | --- |
| D1 | Self-referential movable records | major | planned | D3 |
| D2 | Full lifetime elision/inference | major | planned | felt across all examples |
| D3 | Async / concurrency | major | planned | R8, D1 |
| D4 | High-performance optimizer | minor | planned | — |
| D5 | Full C ABI coverage | major | planned | R5, R6 |
| D6 | Full DWARF debug info | minor | planned | — |
| D7 | Package registry | minor | planned | — |
| D8 | Traits / interfaces (abstraction, generic bounds) | major | planned | R11, R13 |

---

## R1 — Recursive and mutually-recursive functions

Severity: major. Status: proposed.

Discovered while writing the `calc_vm.cdb` bytecode interpreter: a tree-walking
evaluator over a recursive `box<Node>` AST is the natural design, but a function
body cannot reference its own symbol (it fails resolution with
`unknown function <name>`), and there is no mutual recursion across functions.
The program had to be reshaped into a single `fold` over a *flat* instruction
stream instead.

Gap:

```text
a function body cannot call the function it defines (direct recursion)
two functions cannot call each other (mutual recursion)
the only iteration construct is `fold item in target with acc = init do body`
  over fixed arrays and slices (see PLAN_V2 Phase 13)
```

Impact:

```text
no tree/graph traversal (even though recursive box<T> *types* already exist,
  no function can walk one)
no recursive-descent parsing
no divide-and-conquer algorithms
forces every iterative program into a flat fold, even when the data is nested
```

Current workaround:

```text
flatten the problem into a sequence and `fold` over it
  (works for stack VMs and stream processors; not for trees)
precompute a bound and carry an explicit stack/worklist in the accumulator
```

Proposed direction (design notes, not committed):

```text
allow a function's own symbol — and forward-declared peers — in scope inside bodies
the interesting constraint is content addressing: a function that embeds its own
  hash is a cycle in the object DAG, so recursion needs a fixpoint/by-name
  reference marker rather than a direct hash edge
no totality/termination requirement (native target), but verify must handle
  recursive call graphs for effects, borrows, moves, and drop ordering
consider an explicit recursion form or a recursion-group object so the content
  hash of a recursive clique is well-defined and replayable
```

Acceptance (when promoted to a phase):

```text
a native-required fixture using recursion compiles to a native artifact
  (e.g. sum/length over a recursive box<Node>, or a tree-walking expr evaluator)
the reference evaluator agrees as an oracle
verify covers recursive call graphs; replay/export/import round-trips
```

---

## R2 — Remainder / modulo operator

Severity: minor. Status: proposed.

Discovered while writing the decimal printer in `calc_vm.cdb`: extracting a digit
needs `n mod 10`, but the parser rejects `%` (`expected fn, got Symbol("%")`).
Only `+ - * /`, comparisons, and `&& ||` are available.

Gap:

```text
no `%` token and no remainder/modulo binary operator
```

Impact:

```text
digit extraction, parity checks, ring-buffer indexing, hashing, alignment math,
  and calendar math all require an awkward identity instead of one operator
```

Current workaround:

```text
r = a - (a / b) * b      // used in calc_vm.cdb's print_nonneg digit fold
```

Proposed direction:

```text
add `%` as a binary operator everywhere `/` is already handled:
  parser, type rules, evaluator, lowering, native backend, trace, verify
define negative-operand semantics explicitly (match `/`'s rounding)
trap on modulo-by-zero with the same machinery as divide-by-zero
```

Acceptance (when promoted to a phase):

```text
`eval` of `%` matches the chosen semantics, including negative operands
a native fixture computes the same result as the evaluator
modulo-by-zero traps with parity to the existing `/` trap
```

---

## R3 — Integer→string formatting in the standard library

Severity: major. Status: proposed.

Closely related to R2 and found in the same exercise: there is no library way to
turn an `i64` into printable text. `calc_vm.cdb` had to hand-roll decimal
conversion as a `fold` over place values, mapping each digit through a static
`b"0123456789"` table, with manual leading-zero suppression and sign handling.

Gap:

```text
no std int→string / int→bytes formatting
no general numeric formatting (decimal/hex/unsigned/width/sign)
no inverse: no bytes→int parsing for reading numeric input
```

Impact:

```text
any CLI that reports a computed number must reinvent decimal conversion
this directly limits PLAN_V2's "useful programs" goal: stdout is the main output
  channel, yet numbers cannot be printed without bespoke code
```

Current workaround:

```text
the print_nonneg / print_i64 digit fold in calc_vm.cdb
  (place-value array + static ASCII digit table + manual zero suppression)
```

Proposed direction:

```text
a `std.fmt` module building on the Phase 20 dynamic string/buffer surface:
  i64 -> string (and a write-to-buffer variant to avoid per-digit syscalls)
  add unsigned and hex variants as follow-ups
the parsing inverse (bytes -> i64) to make numeric *input* practical
```

Acceptance (when promoted to a phase):

```text
a native fixture prints a computed number via `std.fmt` with no hand-rolled table
format/parse round-trips for a range of values including negatives
```

---

## R4 — Bitwise operators

Severity: major. Status: proposed.

Discovered while writing `fnv1a.cdb`: FNV-1a is `hash = (hash XOR byte) * prime`,
but there is no `^`, and no `& | ~ << >>` either — the parser rejects all of
them. Hex output also wants `>> 4` / `& 0xf` per nibble.

Gap:

```text
no bitwise AND, OR, XOR, NOT, shift-left, or shift-right operators
```

Impact:

```text
hashing, checksums, hex/base64 codecs, compression, bitsets/flags, and crypto
  are blocked or expressible only by emulation
```

Current workaround:

```text
emulate per bit with division and comparison folds — see `xor_low_byte` in
  fnv1a.cdb, which XORs one byte with an 8-iteration fold; masks and shifts
  become `x - (x/p)*p` and `x / p`
```

Proposed direction:

```text
add `& | ^ ~ << >>` everywhere arithmetic operators are handled
  (parser, types, eval, lowering, native backend, trace, verify)
define out-of-range shift behavior; define width interaction with R5 types
```

Acceptance (when promoted to a phase):

```text
a native fixture computes a hash/codec with real bitwise ops; oracle agrees
fnv1a.cdb drops its xor_low_byte emulation and still produces 0x1e225c96
```

---

## R5 — Fixed-width and unsigned integer types, and wrapping arithmetic

Severity: major. Status: proposed.

Also from `fnv1a.cdb`: FNV-1a is defined on a 32-bit unsigned word with wrapping
multiply, but the only integer types are `i64` and `u8`. There is no `u32`/`u64`/
`i32`, and no wrapping/defined-overflow arithmetic.

Gap:

```text
no fixed-width integer types beyond i64 and u8 (no i8/i16/i32/u16/u32/u64)
no unsigned 32/64-bit arithmetic
no wrapping / checked / saturating semantics; overflow behavior unspecified
```

Impact:

```text
correct fixed-width hashing/checksums, C ABI/FFI interop with sized integer
  types, and modular arithmetic all require manual masking
```

Current workaround:

```text
keep everything in i64 and wrap by hand: `wrap32(x) = x - (x / 2^32) * 2^32`
  (relies on i64 being wide enough to hold the pre-wrap product)
```

Proposed direction:

```text
introduce sized integer types (i8/i16/i32/i64, u8/u16/u32/u64) building on the
  existing layout model; define wrapping vs. trapping arithmetic (or explicit
  wrapping operators); integrate with R4 (bitwise) and R6 (casts)
```

Acceptance (when promoted to a phase):

```text
a native fixture using u32 wrapping multiply matches a reference hash
layout/ABI for the new widths is verified and replayable
```

---

## R6 — Numeric casts and conversions

Severity: major (a hard block for byte-input programs). Status: proposed.

The sharpest wall in `fnv1a.cdb`: a `u8` element read from a byte slice cannot
have its numeric value used in `i64` arithmetic. No conversion exists in any
form — `i64(x)`, `x as i64`, `cast(x)`, a named builtin, and implicit widening of
a `u8` parameter were all rejected. Bytes are effectively compare/index-only.

Gap:

```text
no numeric conversion between integer types, in any syntax
a u8's value cannot reach i64 arithmetic; u8 params do not widen
```

Impact:

```text
real byte input (`b"..."`, slice<'a, u8>, string bytes) cannot be processed
  numerically — you cannot hash, sum, or parse the bytes you can read
this is the reason fnv1a.cdb hashes a hand-encoded array<i64> instead of a string
```

Current workaround:

```text
hand-encode the data as `array<i64, N>` byte values, sidestepping real input
  (fnv1a.cdb: "codedb" written as [99,111,100,101,100,98])
```

Proposed direction:

```text
define explicit numeric conversions (widen/narrow/sign) with clear syntax and
  trap-or-wrap rules for narrowing; at minimum u8<->i64; pairs with R5
```

Acceptance (when promoted to a phase):

```text
a native fixture hashes a real `b"..."` by converting its bytes to the
  arithmetic domain; the oracle agrees; fnv1a.cdb hashes a string literal
  instead of a hand-coded i64 array
```

---

## R7 — Early exit and error control flow

Severity: major. Status: probed (not yet exercised by a built program).

Found by operator probe, not yet forced by a fixture: there is no `return`, and
`break`/`continue` are reserved and unsupported (PLAN_V2 Phase 13). A function or
`fold` cannot stop early, so error handling and search must thread a validity
flag through accumulator state and keep iterating.

Gap:

```text
no early `return`; no `break`/`continue`; no `?`-style error propagation
```

Impact:

```text
malformed-input handling, short-circuit search, and "first match" iteration must
  run to completion carrying a done/ok flag, distorting otherwise simple loops
```

Current workaround:

```text
carry an explicit `ok: bool` / `done: bool` in the fold accumulator and ignore
  later iterations once it flips
```

Proposed direction:

```text
scoped `break`/`continue` for fold (Phase 13 reserved them), and/or a Result
  type with `?` propagation; define interaction with drop ordering and effects
```

Acceptance (when promoted to a phase):

```text
a native fixture that rejects malformed input exits its loop early
verify covers drop/borrow correctness across the early-exit edge
```

---

## R8 — Unbounded iteration and long-running loops

Severity: major. Status: proposed.

Discovered while writing `http_server.cdb`: a server's accept loop is
conceptually `for(;;)`, but the only iteration construct is `fold` over a finite
array/slice (PLAN_V2 Phase 13), and there is no recursion (R1). There is no
`while`, no `loop`, and no way to iterate until an external condition.

Gap:

```text
no `while cond do ...`, no `loop { ... }`, no unbounded recursion
iteration count is fixed by the length of the array/slice being folded
```

Impact:

```text
servers, daemons, event loops, REPLs, retry-until-success, and poll-until-ready
  cannot be written — the program must know its iteration count up front
http_server.cdb serves exactly 16 connections (a fold over a 16-element array)
  and then exits
```

Current workaround:

```text
fold over a fixed-size array to bound the loop; pick the bound large enough
```

Proposed direction:

```text
a `while cond do body` and/or `loop { ... break }` construct (pairs with R7's
  break); native fold already lowers to real loops (Phase 13), so a
  condition-driven loop is a backend-feasible extension
verify must handle potentially non-terminating control flow and loop-carried
  borrows/drops/effects
```

Acceptance (when promoted to a phase):

```text
a native fixture runs an accept loop until a shutdown condition
http_server.cdb serves until killed instead of a fixed count
```

---

## R9 — Array fill / repeat initializer

Severity: minor. Status: proposed.

Found in `http_server.cdb`, and previously visible in `todo_cli.cdb`: a fixed
array literal must list every element. `[expr; N]` is rejected by the parser, so
a large zeroed buffer is impractical to write as a value.

Gap:

```text
no `[value; N]` repeat initializer for arrays; every element must be listed
```

Impact:

```text
large fixed buffers cannot be built as values (a 1024-byte zero buffer would be
  1024 literal elements); todo_cli.cdb hand-lists 32 zeros for a 32-byte buffer
```

Current workaround:

```text
heap-allocate with malloc (http_server.cdb uses malloc(1024, 16) for the read
  buffer), or hand-list a small array
```

Proposed direction:

```text
add `[expr; N]` repeat initializer, lowering to a fill/memset over the array place
```

Acceptance (when promoted to a phase):

```text
`[0; 1024]` type-checks and lowers; http_server.cdb uses a stack array buffer
  instead of malloc
```

---

## R10 — Floating-point numbers

Severity: major. Status: probed.

Found by roadmap review: the only numeric types are `i64` and `u8` — there is no
float type at all.

Gap:

```text
no f32/f64 type, no float literals (3.14), no float arithmetic or int<->float casts
```

Impact:

```text
no real-number computation — averages, geometry, signal/scientific, statistics,
  graphics, ML, or decimal money; arithmetic is integer-only
```

Current workaround:

```text
hand-rolled fixed-point in i64 (scale by a power of ten), with manual rounding
```

Proposed direction:

```text
add f32/f64 types, literals, IEEE arithmetic/comparison, and int<->float
  conversions (pairs with R6); lower to native float instructions
```

Acceptance (when promoted to a phase):

```text
a native fixture computes and prints a floating-point result matching a reference
```

---

## R11 — Generics / parametric polymorphism

Severity: major. Status: probed.

Found by roadmap review: functions, records, and enums accept only lifetime
parameters; a type parameter `<T>` is rejected.

Gap:

```text
no generic functions or types; no Option<T>, Result<T, E>, or a generic container
```

Impact:

```text
data structures and helpers cannot be reused across element types; the stdlib
  hand-monomorphizes (std.result.IoResult is i64-only); no generic optional/error
```

Current workaround:

```text
duplicate code per concrete type, or erase everything to i64 / bytes
```

Proposed direction:

```text
type parameters on fn/record/enum with monomorphization at lowering; start
  constraint-free, add bounds/traits later
```

Acceptance (when promoted to a phase):

```text
one generic Option<T> (or Vec<T>) definition compiles natively at two or more
  instantiations
```

---

## R12 — Process arguments and environment

Severity: major. Status: probed (already flagged deferred in PLAN_V2 Phase 19/22).

Found by roadmap review and `todo_cli.cdb`, which notes "argv remains deferred":
the entry point is `main() -> i64` with no access to command-line arguments.

Gap:

```text
no argv (or envp); a program cannot read its arguments, flags, or environment
```

Impact:

```text
CLIs cannot take input — the defining feature of a CLI; todo_cli.cdb hard-codes
  its file paths because it cannot receive them
```

Current workaround:

```text
hard-code inputs, or read from a fixed, known file path
```

Proposed direction:

```text
thread target argv/envp into a richer entry signature (e.g. main(args: slice<...>))
  per the PLAN_V2 deferred-argv note
```

Acceptance (when promoted to a phase):

```text
a native fixture echoes its first command-line argument
```

---

## R13 — First-class functions

Severity: major. Status: probed.

Found by roadmap review: a function-typed parameter (`f: fn(i64) -> i64`) is
rejected. Functions are not values; only the built-in `fold` takes a body.

Gap:

```text
no function-pointer/closure types, no passing or returning functions, no
  user-defined higher-order functions (map/filter/callbacks/dispatch tables)
```

Impact:

```text
no callbacks, strategies, visitors, or user-defined iteration combinators; fold
  is the only higher-order construct and it is a builtin
```

Current workaround:

```text
enum-tag + `case` dispatch in place of passing a function
```

Proposed direction:

```text
function-pointer types and indirect call first; capturing closures later, with
  care around borrows/moves of captured values
```

Acceptance (when promoted to a phase):

```text
a native fixture passes a named function as an argument and calls it indirectly
```

---

## R14 — Pattern matching richness

Severity: minor. Status: probed.

Found by roadmap review: `case` matches only enum variants. Literal patterns, the
`_` wildcard, `if` guards, and nested patterns are all rejected.

Gap:

```text
no literal/range patterns, no `_` wildcard, no guards, no nested destructuring
```

Impact:

```text
integer/byte dispatch falls back to if/else chains (used throughout calc_vm.cdb
  and fnv1a.cdb); deep matches need manual field access
```

Current workaround:

```text
if/else chains for literal dispatch; manual `.field` access or re-`case` nesting
```

Proposed direction:

```text
extend `case` with literal/wildcard/guard/nested patterns plus exhaustiveness
```

Acceptance (when promoted to a phase):

```text
a native fixture dispatches on integer literals with a `_` default and a nested
  pattern
```

---

## R15 — String operations

Severity: major. Status: probed.

Found by roadmap review: the `string` type has only `string_new` and
`string_len`. It cannot be indexed, compared, concatenated, sliced, or appended
to — it is effectively opaque once created.

Gap:

```text
no string indexing, equality, concatenation (++), substring/slice, push/build,
  or bytes<->string conversion; only string_new + string_len exist
```

Impact:

```text
text cannot be transformed or assembled at the `string` level; programs drop down
  to raw byte slices and folds (word_count.cdb) and cannot build output strings
```

Current workaround:

```text
scan with folds over slice<'a, u8>; assemble bytes in a buffer and write them
  directly (calc_vm/fnv1a print digit-by-digit rather than build a string)
```

Proposed direction:

```text
a `std.string` surface over the Phase 20 dynamic-buffer foundation: index,
  compare, concat, substring, push, and bytes<->string; pairs with R3 (formatting)
```

Acceptance (when promoted to a phase):

```text
a native fixture concatenates two strings, compares them, and indexes a byte
```

---

## Also observed (minor, not yet numbered)

Found while writing the examples and during the roadmap review, recorded so they
are not lost; promote to numbered items if a future program is blocked on them:

```text
no null-pointer literal — `accept(fd, addr, addrlen)` was given real malloc'd
  throwaway buffers because there is no way to pass NULL
no typed-struct -> raw FFI bridge — sockaddr_in is a hand-laid byte literal
  rather than a `record` whose address is passed to the syscall (also blocked by
  R4/R5/R6: the numeric fields need byte-order/width control to fill correctly)
no const/static declarations — constants are nullary functions (`fnv_prime()`,
  `two_32()`); workable but verbose, and one const cannot be built from another
no type aliases — `type Byte = u8` is rejected, so long structural types repeat
```

## Deferred from the v2 plan (planned, D-series)

The R-series above was found by accident. The D-series is its deliberate
counterpart: capabilities the v2 plan named as non-goals up front
([PLAN_V2.md](PLAN_V2.md) "Out of scope for initial v2", [SPEC_V2.md](SPEC_V2.md)
§22), now tracked as planned future work. (D1–D7 come from that list; D8 — traits
— is review-added, an architectural gap the list omits.) Each is cross-linked to
the discovered R-items that are facets of it. Like the R-series, a D-item is "resolved" only
with a native-required acceptance fixture.

### D1 — Self-referential movable records

Severity: major. Status: planned (deferred from initial v2).

A record cannot hold a reference into its own storage and remain movable: a move
relocates the storage and dangles the internal reference, so the borrow/move
checker forbids it. `box<T>` indirection (which works — `box_heap.cdb`) covers
recursive *shape*, but not intrusive self-references.

- Why deferred: needs pin / move-stability analysis; it is also the
  representation behind async state machines.
- Relation: D3 (async), and the move/drop model (PLAN_V2 Phase 8).
- Direction: an immovable/pin marker, or a move-stability analysis that keeps the
  internal reference valid across moves.
- Acceptance: a native fixture builds a struct with an internal back-reference;
  the checker pins it or rejects the dangling move.

### D2 — Full lifetime elision/inference

Severity: major (ergonomic, pervasive). Status: planned (deferred from initial v2).

v2 chose explicit region parameters, so every function that names a
`slice<'a, T>` or `&'a T` must declare and thread `<'a>` — even when the region
never escapes the body. All three examples are littered with it (`run<'a>`,
`print_nonneg<'a>`, `serve<'a>`); it is the one deferral that bit on every single
program.

- Why deferred: SPEC_V2 open question — "how much region inference vs. explicit
  region parameters."
- Relation: felt pervasively; no R-item minted because the plan already names it.
- Direction: elide lifetimes in the common non-escaping / single-region cases;
  fuller inference later.
- Acceptance: the example programs drop most `<'a>` annotations and still verify
  and build identically.

### D3 — Async / concurrency

Severity: major. Status: planned (deferred from initial v2).

No threads, no async, no way to handle connections in parallel. Combined with R8
(no unbounded loop), there is no event loop at all.

- Why deferred: large; needs a runtime/scheduling model and self-referential
  state machines (D1).
- Relation: R8 (the loop is the prerequisite), D1 (async state is the canonical
  self-referential movable record).
- Direction: start with OS threads or a minimal poll/kqueue event loop;
  async/await later.
- Acceptance: a native fixture HTTP server handles two overlapping connections.

### D4 — High-performance optimizer

Severity: minor (quality, not expressiveness). Status: planned (deferred from initial v2).

v2 lowering is correctness-first; there is no real optimization pipeline
(inlining, strong const-folding, register-allocation quality, etc.).

- Why deferred: v2 prioritized correct native artifacts over speed.
- Relation: — (no example probed performance).
- Direction: an optimization-pass layer over the lowered IR, gated by a
  verified-identical-results check and a benchmark suite.
- Acceptance: a benchmark fixture improves measurably with identical results and
  passing verify.

### D5 — Full C ABI coverage

Severity: major. Status: planned (deferred from initial v2).

FFI today handles scalar and pointer arguments. There is no struct-by-value
passing/returning, no sized-integer ABI guarantee (int-returning libc calls are
declared `-> i64`), and no typed-struct → raw-pointer bridge — which is why
`http_server.cdb` hand-lays `sockaddr_in` as raw bytes.

- Why deferred: full SysV / AAPCS64 struct classification is a large, per-target
  effort.
- Relation: R5 (fixed-width ints), R6 (numeric casts), and the "also observed"
  struct→FFI note.
- Direction: implement per-target struct classification; sized-integer return
  types; pass/return small structs by value.
- Acceptance: a native fixture passes and returns a C `struct` by value across
  the boundary, checked against a C reference.

### D6 — Full DWARF debug info

Severity: minor. Status: planned (deferred from initial v2).

Semantic-level debug metadata exists (provenance/blame, native debug metadata
tests), but there is no full DWARF, so source-level debugging in lldb/gdb on a
built binary is not available.

- Why deferred: large and target-specific.
- Relation: — (not probed).
- Direction: emit DWARF mapping native PCs to semantic locations and types.
- Acceptance: lldb sets a breakpoint and inspects a variable in a built binary.

### D7 — Package registry

Severity: minor. Status: planned (deferred from initial v2).

Object import/export exists, but there is no package/dependency distribution;
modules live inside a single database.

- Why deferred: distribution comes after the language/compiler foundations.
- Relation: — (not probed); the content-addressed object store is a natural fit.
- Direction: a content-addressed package/registry layer over the object store
  with explicit dependency resolution.
- Acceptance: a second database depends on a published package and builds against
  it.

### D8 — Traits / interfaces

Severity: major. Status: planned.

There is no way to abstract over behavior: no traits, interfaces, or typeclasses,
no bounds to constrain generics (R11), and no dynamic dispatch. Unlike D1–D7,
traits are *not* on the v2 plan's non-goals list — this is a review-found
architectural gap, which is itself the point of the takeaway below.

- Why now: the natural follow-on to generics — bounded/constrained generics need
  a trait system — so it sits directly behind R11.
- Relation: R11 (generics: traits are the constraint system), R13 (first-class
  functions: a complementary axis of polymorphism).
- Direction: trait declarations + impls with static (monomorphized) dispatch
  first; dynamic dispatch (vtables) later.
- Acceptance: a native fixture defines a trait, implements it for two types, and
  calls it generically.

`GC` and cross-machine distributed exchange are also on the PLAN_V2 list but are
intentional architectural *stances* (v2 is compiler-owned-memory by design), not
future features, so they are not tracked as D-items.

Takeaway: the plan's non-goals are all large and architectural, yet writing real
programs tripped almost entirely over small, basic-expressiveness gaps
(operators, casts, an unbounded loop, array fill — R2–R9) that **no deferral list
names** and that are each individually cheap. The two series barely intersect —
only D2 (lifetime elision) and D5 (C ABI) connect to anything the programs hit.
Right now it is the un-deferred small gaps, not the famous big deferrals, that
block useful programs. And a review pass (R10–R15) then surfaced *large*
capabilities the non-goals list also never names — floating point, generics,
argv, first-class functions — so the formal deferral list is not a complete map
of what is missing.

## Candidate programs to probe next

New roadmap items come from new programs. Built so far:

```text
examples/calc_vm.cdb     (stack VM)     -> seeded R1, R2, R3
examples/fnv1a.cdb       (FNV-1a)       -> seeded R4, R5, R6  (and probe -> R7)
examples/http_server.cdb (TCP/HTTP/1.1) -> seeded R8, R9
```

Good remaining targets, chosen to stress axes the two demos did *not*:

```text
byte-stream calculator / tokenizer — string scanning, char classes, and the
  first program that would actually FORCE R7 (early-exit on malformed input);
  also exercises R6 (parse bytes->int) on real input
CSV / ledger summarizer — bytes->int parsing (R3 inverse), multi-field state,
  per-column accumulation, dynamic Vec growth
text report/table formatter — int->string (R3), string concatenation, padding
```

Pick one, build it natively, and append every workaround it forces as a new
`R<n>` item above.
