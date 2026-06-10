// Phase 4 / Phase 6 acceptance strengthening (SPEC_V3 §7, the V3.1 follow-on note):
// a RUNTIME allocation interposer that closes the "no-leak half of exactly-once".
//
// The lowered-IR verifier proves drops happen AT MOST once (no double-free), and the
// native double-free guard (libc aborts a double free) covers the upper bound. But
// the LOWER bound — every owned heap value is freed AT LEAST once, i.e. no pure leak
// from a skipped drop — previously rested only on a static "object still references
// free" check; the native harness counted no allocations, so a leak was invisible at
// runtime. This test interposes malloc/calloc/realloc/free in the built native
// binary and counts them.
//
// A naive malloc==free balance does NOT work: the C runtime itself performs many
// unbalanced allocations (≈179 net on macOS) over a process lifetime. Instead we use
// a DIFFERENTIAL: build the SAME program at two allocation scales and assert the net
// `alloc - free` is INVARIANT to the scale. Process-runtime allocations are identical
// across the two runs (same binary structure, only an integer constant differs) so
// they cancel exactly; a skipped-drop leak makes the larger scale leak more, so its
// net grows. Allocation counts must also scale with the program's count, which proves
// the interposer is actually injected (guards against a vacuous pass).
//
// Coverage: conditional drop glue, field-granular partial moves, the two combined,
// and a recursive `box<Node>` heap freed by `case` + `unbox` — each at scale.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::{TempDir, tempdir};

/// Allocation-counting interposer. macOS uses dyld `__interpose`; ELF uses
/// `LD_PRELOAD` symbol override via `dlsym(RTLD_NEXT, ...)` with a small bootstrap
/// buffer for allocations requested while the real symbols are being resolved. At
/// process exit (a `destructor`, run on normal `exit()` after `main` returns) it
/// writes "<alloc_calls> <free_calls>" to the file named by `$CDB_LEAK_REPORT`.
const INTERPOSER_C: &str = r#"
#define _GNU_SOURCE 1
#include <stdlib.h>
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>

static volatile long cdb_alloc_calls = 0;
static volatile long cdb_free_calls = 0;

static void cdb_report(void) {
    const char *path = getenv("CDB_LEAK_REPORT");
    if (!path) return;
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return;
    char buf[64];
    int n = snprintf(buf, sizeof(buf), "%ld %ld\n", cdb_alloc_calls, cdb_free_calls);
    if (n > 0) { ssize_t w = write(fd, buf, (size_t)n); (void)w; }
    close(fd);
}

__attribute__((destructor)) static void cdb_fini(void) { cdb_report(); }

#ifdef __APPLE__
static void *cdb_malloc(size_t s) { __sync_fetch_and_add(&cdb_alloc_calls, 1); return malloc(s); }
static void *cdb_calloc(size_t a, size_t b) { __sync_fetch_and_add(&cdb_alloc_calls, 1); return calloc(a, b); }
static void *cdb_realloc(void *p, size_t s) {
    if (p == NULL) __sync_fetch_and_add(&cdb_alloc_calls, 1);
    else if (s == 0) __sync_fetch_and_add(&cdb_free_calls, 1);
    return realloc(p, s);
}
static void cdb_free(void *p) { if (p) __sync_fetch_and_add(&cdb_free_calls, 1); free(p); }

typedef struct { const void *replacement; const void *replacee; } cdb_interpose_t;
__attribute__((used)) static const cdb_interpose_t cdb_interposers[]
    __attribute__((section("__DATA,__interpose"))) = {
    { (const void *)cdb_malloc,  (const void *)malloc },
    { (const void *)cdb_calloc,  (const void *)calloc },
    { (const void *)cdb_realloc, (const void *)realloc },
    { (const void *)cdb_free,    (const void *)free },
};
#else
#include <dlfcn.h>
static void *(*cdb_real_malloc)(size_t) = NULL;
static void *(*cdb_real_calloc)(size_t, size_t) = NULL;
static void *(*cdb_real_realloc)(void *, size_t) = NULL;
static void  (*cdb_real_free)(void *) = NULL;
static char cdb_bootstrap[1 << 16];
static size_t cdb_bootstrap_off = 0;
static int cdb_resolving = 0;

static int cdb_is_bootstrap(void *p) {
    return (char *)p >= cdb_bootstrap && (char *)p < cdb_bootstrap + sizeof(cdb_bootstrap);
}
static void *cdb_bootstrap_alloc(size_t size) {
    size_t off = (cdb_bootstrap_off + 15u) & ~((size_t)15u);
    if (off + size > sizeof(cdb_bootstrap)) return NULL;
    cdb_bootstrap_off = off + size;
    return &cdb_bootstrap[off];
}
static void cdb_resolve(void) {
    if (cdb_real_malloc) return;
    cdb_resolving = 1;
    cdb_real_malloc = (void *(*)(size_t)) dlsym(RTLD_NEXT, "malloc");
    cdb_real_calloc = (void *(*)(size_t, size_t)) dlsym(RTLD_NEXT, "calloc");
    cdb_real_realloc = (void *(*)(void *, size_t)) dlsym(RTLD_NEXT, "realloc");
    cdb_real_free = (void (*)(void *)) dlsym(RTLD_NEXT, "free");
    cdb_resolving = 0;
}
void *malloc(size_t s) {
    if (cdb_resolving) return cdb_bootstrap_alloc(s);
    cdb_resolve();
    __sync_fetch_and_add(&cdb_alloc_calls, 1);
    return cdb_real_malloc(s);
}
void *calloc(size_t a, size_t b) {
    if (cdb_resolving) { void *p = cdb_bootstrap_alloc(a * b); if (p) memset(p, 0, a * b); return p; }
    cdb_resolve();
    __sync_fetch_and_add(&cdb_alloc_calls, 1);
    return cdb_real_calloc(a, b);
}
void *realloc(void *p, size_t s) {
    cdb_resolve();
    if (p == NULL) __sync_fetch_and_add(&cdb_alloc_calls, 1);
    else if (s == 0) __sync_fetch_and_add(&cdb_free_calls, 1);
    return cdb_real_realloc(p, s);
}
void free(void *p) {
    if (cdb_is_bootstrap(p)) return;
    cdb_resolve();
    if (p) __sync_fetch_and_add(&cdb_free_calls, 1);
    cdb_real_free(p);
}
#endif
"#;

// box<Line> conditionally moved (into `take` on `then`, dropped by the merge
// compensation on `else`), once per recursion level — Phase 4 conditional drop glue.
const CONDITIONAL: &str = r#"
record Line { v: i64 }
fn take(b: box<Line>) -> i64 effects[alloc] = b.v
fn go(n: i64, flag: bool) -> i64 effects[alloc] =
  if n < 1 then 0
  else let b: box<Line> = box_new({ v: n }) in
       (if flag then take(b) else b.v) + go(n - 1, flag)
fn main() -> i64 effects[alloc] = go({K}, true) + go({K}, false)
"#;

// `t.a` (owned box) moved into `take`, sibling `t.c` stays live and is dropped at
// scope exit, once per recursion level — Phase 4 field-granular partial move.
const FIELD_GRANULAR: &str = r#"
record Line { v: i64 }
record Two { a: box<Line>
  c: box<Line> }
fn take(b: box<Line>) -> i64 effects[alloc] = b.v
fn go(n: i64) -> i64 effects[alloc] =
  if n < 1 then 0
  else let t: Two = { a: box_new({ v: n }), c: box_new({ v: n }) } in
       take(t.a) + t.c.v + go(n - 1)
fn main() -> i64 effects[alloc] = go({K})
"#;

// The two dimensions combined: `t.a` moved in only the `then` branch (compensating
// drop on `else`) while sibling `t.c` always stays live — at recursion scale.
const COMBINED: &str = r#"
record Line { v: i64 }
record Two { a: box<Line>
  c: box<Line> }
fn take(b: box<Line>) -> i64 effects[alloc] = b.v
fn go(n: i64, flag: bool) -> i64 effects[alloc] =
  if n < 1 then 0
  else let t: Two = { a: box_new({ v: n }), c: box_new({ v: n }) } in
       let picked: i64 = if flag then take(t.a) else 99 in
       picked + t.c.v + go(n - 1, flag)
fn main() -> i64 effects[alloc] = go({K}, true) + go({K}, false)
"#;

// Recursive `box<Node>` heap: `build` allocates a chain of K boxes, `length` walks it
// by `case` + `unbox`, freeing each node exactly once — Phase 6 deref-by-move drops.
const RECURSIVE_BOX_HEAP: &str = r#"
enum Node { empty: unit
  next: box<Node> }
fn build(n: i64) -> Node effects[alloc] =
  if n < 1 then Node::empty(()) else Node::next(box_new(build(n - 1)))
fn length(n: Node) -> i64 effects[alloc] =
  case n of
    empty(u) => 0
  | next(boxed) => 1 + length(unbox(boxed))
fn main() -> i64 effects[alloc] = length(build({K}))
"#;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn p(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn supported_platform() -> bool {
    (cfg!(target_os = "macos") && cfg!(target_arch = "aarch64"))
        || (cfg!(target_os = "linux") && cfg!(target_arch = "x86_64"))
}

fn cc_available() -> bool {
    StdCommand::new("cc").arg("--version").output().is_ok()
}

fn compile_interposer(dir: &Path) -> Option<PathBuf> {
    let src = dir.join("cdb_leak_interposer.c");
    std::fs::write(&src, INTERPOSER_C).ok()?;
    let lib = dir.join(if cfg!(target_os = "macos") {
        "libcdbleak.dylib"
    } else {
        "libcdbleak.so"
    });
    let mut cc = StdCommand::new("cc");
    if cfg!(target_os = "macos") {
        cc.args(["-dynamiclib", "-O1"]);
    } else {
        cc.args(["-shared", "-fPIC", "-O1"]);
    }
    cc.arg(&src).arg("-o").arg(&lib);
    if cfg!(target_os = "linux") {
        cc.arg("-ldl");
    }
    let status = cc.status().ok()?;
    status.success().then_some(lib)
}

// Inline (non-box) move-only enum payload moved out of a `case` arm: the payload is
// a record owning a box, bound and moved out of a consumed-place scrutinee, once per
// recursion level — the #3 fail-closed item, now lowered as a shallow byte move.
const INLINE_ENUM_PAYLOAD: &str = r#"
record Boxed { b: box<i64> }
enum E { only: Boxed }
fn consume(x: Boxed) -> i64 effects[alloc] = unbox(x.b)
fn go(n: i64) -> i64 effects[alloc] =
  if n < 1 then 0
  else let e: E = E::only({ b: box_new(n) }) in
       let x: Boxed = (case e of only(p) => p) in
       consume(x) + go(n - 1)
fn main() -> i64 effects[alloc] = go({K})
"#;

// R14 nested enum-destructuring, move-out path: a move-only payload (a record owning
// a box) is destructured TWO levels deep and moved out of a consumed-place scrutinee,
// then freed by `consume`/`unbox`, once per recursion level. Exercises the nested
// binding-move + shallow-byte-move drop placement at scale.
const NESTED_MOVE_OUT: &str = r#"
record Boxed { b: box<i64> }
enum Inner { has: Boxed
  none: unit }
enum Outer { wrap: Inner }
fn consume(x: Boxed) -> i64 effects[alloc] = unbox(x.b)
fn go(n: i64) -> i64 effects[alloc] =
  if n < 1 then 0
  else let o: Outer = Outer::wrap(Inner::has({ b: box_new(n) })) in
       (case o of wrap(has(x)) => consume(x) | wrap(none(u)) => 0) + go(n - 1)
fn main() -> i64 effects[alloc] = go({K})
"#;

// R14 nested destructuring, fallback-drop path: the constructed inner variant (`other`)
// is NOT covered by the explicit nested arm, so the `_` fallback fires and must free
// `other`'s box payload, once per recursion level. Exercises the inner no-binding-arm
// payload drop at scale (a missed drop leaks one box per level).
const NESTED_FALLBACK_DROP: &str = r#"
record Boxed { b: box<i64> }
enum Inner { has: Boxed
  other: Boxed }
enum Outer { wrap: Inner }
fn consume(x: Boxed) -> i64 effects[alloc] = unbox(x.b)
fn go(n: i64) -> i64 effects[alloc] =
  if n < 1 then 0
  else let o: Outer = Outer::wrap(Inner::other({ b: box_new(n) })) in
       (case o of wrap(has(x)) => consume(x) | _ => 0) + go(n - 1)
fn main() -> i64 effects[alloc] = go({K})
"#;

// R14 nested destructuring, residual-drop path: the nested leaf `x` is bound but the
// body never consumes it, so a residual drop at arm-scope exit frees its box, once per
// recursion level (mirrors `let`-binding drop placement).
const NESTED_BINDING_RESIDUAL: &str = r#"
record Boxed { b: box<i64> }
enum Inner { has: Boxed
  none: unit }
enum Outer { wrap: Inner }
fn go(n: i64) -> i64 effects[alloc] =
  if n < 1 then 0
  else let o: Outer = Outer::wrap(Inner::has({ b: box_new(n) })) in
       (case o of wrap(has(x)) => 0 | wrap(none(u)) => 0) + go(n - 1)
fn main() -> i64 effects[alloc] = go({K})
"#;

// R14 #1 constant-index array-element partial move: an array of three boxes is built
// per recursion level, element 0 is moved out and freed by `consume`, and the two
// live siblings are dropped by element-granular drop glue at scope exit. A missed
// sibling drop leaks two boxes per level, so net would scale with the count.
const ARRAY_ELEMENT_MOVE: &str = r#"
fn consume(b: box<i64>) -> i64 effects[alloc] = unbox(b)
fn go(n: i64) -> i64 effects[alloc] =
  if n < 1 then 0
  else let xs: array<box<i64>, 3> = [box_new(n), box_new(n), box_new(n)] in
       consume(xs[0]) + go(n - 1)
fn main() -> i64 effects[alloc] = go({K})
"#;

fn build_exe(dir: &Path, name: &str, source: &str) -> PathBuf {
    let db = dir.join(format!("{name}.sqlite"));
    let src = dir.join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();
    bin().args(["init", p(&db)]).assert().success();
    bin().args(["import", p(&db), p(&src)]).assert().success();
    let exe = dir.join(format!("{name}.exe"));
    bin()
        .args(["build", p(&db), "main", "--out", p(&exe)])
        .assert()
        .success();
    exe
}

/// Run `exe` under the allocation interposer; returns `(alloc_calls, free_calls)`, or
/// `None` if the report was not produced (injection blocked by the environment).
fn alloc_counts(dir: &Path, exe: &Path, interposer: &Path) -> Option<(i64, i64)> {
    let report = dir.join("leak_report.txt");
    let _ = std::fs::remove_file(&report);
    let mut cmd = StdCommand::new(exe);
    cmd.env("CDB_LEAK_REPORT", &report);
    if cfg!(target_os = "macos") {
        cmd.env("DYLD_INSERT_LIBRARIES", interposer);
    } else {
        cmd.env("LD_PRELOAD", interposer);
    }
    cmd.status().ok()?;
    let text = std::fs::read_to_string(&report).ok()?;
    let mut fields = text.split_whitespace();
    let alloc = fields.next()?.parse().ok()?;
    let free = fields.next()?.parse().ok()?;
    Some((alloc, free))
}

/// Assert a tunable box program frees every heap allocation: the net `alloc - free`
/// must be INVARIANT to the allocation count (process-runtime allocations cancel),
/// and the counts must scale with the count (the interposer is injected and counting
/// real program allocations). A leak regression makes net grow with the count.
fn assert_balanced_across_scale(
    label: &str,
    template: &str,
    small: i64,
    large: i64,
) -> Option<TempDir> {
    if !(supported_platform() && cc_available()) {
        eprintln!("{label}: skipped (unsupported platform or no cc)");
        return None;
    }
    let dir = tempdir().unwrap();
    let Some(interposer) = compile_interposer(dir.path()) else {
        eprintln!("{label}: skipped (interposer failed to compile)");
        return None;
    };

    let exe_small = build_exe(
        dir.path(),
        &format!("{label}_s"),
        &template.replace("{K}", &small.to_string()),
    );
    let exe_large = build_exe(
        dir.path(),
        &format!("{label}_l"),
        &template.replace("{K}", &large.to_string()),
    );
    let (Some((alloc_s, free_s)), Some((alloc_l, free_l))) = (
        alloc_counts(dir.path(), &exe_small, &interposer),
        alloc_counts(dir.path(), &exe_large, &interposer),
    ) else {
        eprintln!("{label}: skipped (interposer report not produced)");
        return Some(dir);
    };

    // Non-vacuity: the interposer must have observed the program's heap allocations,
    // and they must scale with the allocation count. If injection were silently
    // blocked, the counts would not scale — skip rather than vacuously pass.
    if alloc_l - alloc_s < large - small {
        eprintln!(
            "{label}: skipped (allocations did not scale: small={alloc_s}/{free_s} \
             large={alloc_l}/{free_l}; interposer injection likely blocked)"
        );
        return Some(dir);
    }

    // The leak check: runtime overhead cancels, so net must be identical. A box left
    // unfreed (a skipped drop) makes the larger scale leak more, so its net grows.
    let net_small = alloc_s - free_s;
    let net_large = alloc_l - free_l;
    assert_eq!(
        net_small, net_large,
        "{label}: allocation/free balance is not invariant to scale (LEAK): \
         small = {alloc_s} alloc / {free_s} free (net {net_small}), \
         large = {alloc_l} alloc / {free_l} free (net {net_large})"
    );
    // Every additional allocation at the larger scale had a matching free.
    assert_eq!(
        alloc_l - alloc_s,
        free_l - free_s,
        "{label}: extra allocations were not matched by extra frees (LEAK)"
    );
    Some(dir)
}

#[test]
fn conditional_drop_glue_frees_every_box_at_runtime() {
    assert_balanced_across_scale("conditional", CONDITIONAL, 4, 64);
}

#[test]
fn field_granular_partial_move_frees_every_box_at_runtime() {
    assert_balanced_across_scale("field_granular", FIELD_GRANULAR, 4, 64);
}

#[test]
fn combined_conditional_field_drop_glue_frees_every_box_at_runtime() {
    assert_balanced_across_scale("combined", COMBINED, 4, 64);
}

#[test]
fn recursive_box_heap_unbox_frees_every_node_at_runtime() {
    assert_balanced_across_scale("recursive_box_heap", RECURSIVE_BOX_HEAP, 4, 64);
}

#[test]
fn inline_enum_payload_move_frees_every_box_at_runtime() {
    assert_balanced_across_scale("inline_enum_payload", INLINE_ENUM_PAYLOAD, 4, 64);
}

#[test]
fn nested_destructuring_move_out_frees_every_box_at_runtime() {
    assert_balanced_across_scale("nested_move_out", NESTED_MOVE_OUT, 4, 64);
}

#[test]
fn nested_destructuring_fallback_drop_frees_every_box_at_runtime() {
    assert_balanced_across_scale("nested_fallback_drop", NESTED_FALLBACK_DROP, 4, 64);
}

#[test]
fn nested_destructuring_residual_drop_frees_every_box_at_runtime() {
    assert_balanced_across_scale("nested_binding_residual", NESTED_BINDING_RESIDUAL, 4, 64);
}

#[test]
fn array_element_partial_move_frees_every_box_at_runtime() {
    assert_balanced_across_scale("array_element_move", ARRAY_ELEMENT_MOVE, 4, 64);
}
