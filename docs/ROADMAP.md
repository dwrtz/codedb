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

Severity note: R4, R5, and R6 compound. Together they make hashing, checksums,
and codecs (the program class `fnv1a.cdb` represents) either impossible or
expressible only through arithmetic emulation that no one would ship. R8 is the
single biggest gap for *services*: without an unbounded loop, no server, daemon,
event loop, or REPL can be written — `http_server.cdb` works only by serving a
fixed number of connections and then exiting.

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

## Also observed (minor, not yet numbered)

Found while writing `http_server.cdb`, recorded so they are not lost; promote to
numbered items if a future program is actually blocked on them:

```text
no null-pointer literal — `accept(fd, addr, addrlen)` was given real malloc'd
  throwaway buffers because there is no way to pass NULL
no typed-struct -> raw FFI bridge — sockaddr_in is a hand-laid byte literal
  rather than a `record` whose address is passed to the syscall (also blocked by
  R4/R5/R6: the numeric fields need byte-order/width control to fill correctly)
```

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
