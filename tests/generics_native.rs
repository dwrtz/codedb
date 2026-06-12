// Generics / parametric types (R11, PLAN_V3 Phase 14): type parameters on records
// and enums, monomorphized on demand by substituting concrete type arguments into
// the generic template. A generic instance (`Option<i64>`) is the content hash of a
// `Named` Type object carrying its arguments — its stable derived identity — never a
// separately stored object; its concrete structure (and layout, and lowering) is
// materialized by substitution at use. These tests pin one generic enum at two
// instantiations compiling natively (the acceptance fixture), a generic record, a
// nested generic instance, the import->export->import fixpoint, and the fail-closed
// rejections (arity, higher-kinded use).

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn bin() -> Command {
    Command::cargo_bin("codedb").expect("codedb binary")
}

fn run(args: &[&str]) -> String {
    let output = bin().args(args).assert().success().get_output().clone();
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn run_fail(args: &[&str]) -> String {
    let output = bin().args(args).assert().failure().get_output().clone();
    String::from_utf8(output.stderr).expect("utf8 stderr")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn parse_json(text: &str) -> JsonValue {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("invalid json: {err}\n{text}"))
}

fn can_build_default_native_target() -> bool {
    let native_target = (std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64")
        || (std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64");
    native_target && StdCommand::new("cc").arg("--version").output().is_ok()
}

fn root_hash(db: &Path) -> String {
    let history = parse_json(&run(&["history", path(db), "--json"]));
    history["root_hash"].as_str().expect("root_hash").to_string()
}

/// Import `source`, assert `main` evaluates to `expected`, `verify`s, and (on a
/// buildable host) compiles to a native binary returning the same value — so a
/// generic instance is proven monomorphized identically by the reference evaluator
/// and the native backend (eval == native).
fn check_native(name: &str, source: &str, expected: i64) {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();

    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);
    assert_eq!(
        run(&["eval", path(&db), "main"]).trim(),
        expected.to_string(),
        "{name}: evaluator result"
    );
    run(&["verify", path(&db)]);

    if can_build_default_native_target() {
        let created = parse_json(&run(&[
            "create-test",
            path(&db),
            &format!("{name}_native"),
            "--entry",
            "main",
            "--expect-i64",
            &expected.to_string(),
            "--native-required",
            "--json",
        ]));
        assert_eq!(created["status"], "applied", "{name}: create-test");
        let report = parse_json(&run(&["test", path(&db), "--json"]));
        assert_eq!(report["status"], "passed", "{name}: native test status");
        assert_eq!(report["native_mismatches"], 0, "{name}: native mismatches");
        assert_eq!(
            report["tests"][0]["native"]["comparison"]["actual"]["value"],
            expected.to_string(),
            "{name}: native value"
        );
    }
}

/// Import `source` and assert it is rejected with a message containing `needle` —
/// the fail-closed guards for malformed generic uses.
fn reject(name: &str, source: &str, needle: &str) {
    let temp = tempdir().unwrap();
    let db = temp.path().join(format!("{name}.sqlite"));
    let src = temp.path().join(format!("{name}.cdb"));
    std::fs::write(&src, source).unwrap();
    run(&["init", path(&db)]);
    let err = run_fail(&["import", path(&db), path(&src)]);
    assert!(
        err.contains(needle),
        "{name}: expected rejection containing {needle:?}, got: {err}"
    );
}

const OPTION_TWO_INSTANCES: &str = r#"
enum Option<T> {
  none: unit
  some: T
}

fn unwrap_int(o: Option<i64>, d: i64) -> i64 =
  case o of
    none(u) => d
  | some(x) => x

fn unwrap_bool(o: Option<bool>, d: bool) -> bool =
  case o of
    none(u) => d
  | some(x) => x

fn main() -> i64 =
  let a: i64 = unwrap_int(Option::some(7), 0) in
  let b: bool = unwrap_bool(Option::some(true), false) in
  let c: i64 = unwrap_int(Option::none, 99) in
  if b then a + c else 0
"#;

#[test]
fn generic_option_compiles_natively_at_two_instantiations() {
    // The Phase 14 acceptance fixture: one generic enum (`Option<T>`) compiled
    // natively at two instantiations (`Option<i64>`, `Option<bool>`), eval ==
    // native. `Option::none` infers its argument from the expected parameter type.
    check_native("generic_option", OPTION_TWO_INSTANCES, 7 + 99);
}

const PAIR_RECORD: &str = r#"
record Pair<T> {
  first: T
  second: T
}

fn diff(p: Pair<i64>) -> i64 = p.first - p.second

fn main() -> i64 = diff({ first: 50, second: 8 })
"#;

#[test]
fn generic_record_compiles_natively() {
    check_native("generic_pair", PAIR_RECORD, 42);
}

const BOXED_TWO_INSTANCES: &str = r#"
record Boxed<T> {
  value: T
  tag: i64
}

fn int_tag(b: Boxed<i64>) -> i64 = b.value + b.tag

fn bool_tag(b: Boxed<bool>) -> i64 = b.tag

fn main() -> i64 = int_tag({ value: 10, tag: 5 }) + bool_tag({ value: true, tag: 100 })
"#;

#[test]
fn generic_record_two_instantiations_have_distinct_layouts() {
    // Boxed<i64> (16-byte value+tag) and Boxed<bool> (1-byte value + padding + tag)
    // are distinct monomorphizations with distinct layouts; both run natively.
    check_native("generic_boxed", BOXED_TWO_INSTANCES, 10 + 5 + 100);
}

const NESTED_GENERIC: &str = r#"
enum Option<T> {
  none: unit
  some: T
}
record Pair<T> {
  first: T
  second: T
}
fn get(o: Option<Pair<i64>>) -> i64 =
  case o of
    none(u) => 0
  | some(p) => p.first + p.second
fn main() -> i64 = get(Option::some({ first: 3, second: 4 }))
"#;

#[test]
fn nested_generic_instance_compiles_natively() {
    // `Option<Pair<i64>>` — a generic enum instantiated at a generic record
    // instance. The nested instance is materialized transitively by substitution.
    check_native("generic_nested", NESTED_GENERIC, 7);
}

#[test]
fn generic_instance_blames_back_to_generic_definition() {
    // Provenance: a generic instance type's identity carries the generic's symbol,
    // and `blame-type` on the generic definition reports its birth (the migration
    // that created the parametric definition the instances derive from).
    let temp = tempdir().unwrap();
    let db = temp.path().join("blame.sqlite");
    let src = temp.path().join("blame.cdb");
    std::fs::write(&src, PAIR_RECORD).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let blame = parse_json(&run(&["blame-type", path(&db), "Pair", "--json"]));
    let birth = &blame["birth_migration"];
    assert!(
        birth.is_object(),
        "generic Pair must have a birth migration: {blame}"
    );
    // The generic's birth is the `create_type` migration that introduced the
    // parametric definition every `Pair<...>` instance derives from, and that
    // migration records the type parameters — so blame on an instance traces back
    // to the generic and identifies its parameters.
    assert_eq!(
        birth["operation_kind"], "create_type",
        "generic definition born by create_type: {blame}"
    );
    assert_eq!(
        birth["operation"]["type_params"],
        serde_json::json!(["T"]),
        "the birth migration records the generic's type parameters: {blame}"
    );
}

#[test]
fn generic_program_round_trips_to_a_fixpoint() {
    // import -> export -> import is a fixpoint and the export is byte-stable: a
    // generic definition projects as `enum Option<T>`, uses as `Option<i64>`, and a
    // bare-name `Option::some(..)` whose argument re-infers identically.
    let temp = tempdir().unwrap();
    let db = temp.path().join("rt.sqlite");
    let src = temp.path().join("rt.cdb");
    std::fs::write(&src, OPTION_TWO_INSTANCES).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    // Export the canonical projection, re-import, and re-export.
    let export1 = temp.path().join("rt.export1.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export1)]);

    let db2 = temp.path().join("rt2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export1)]);
    run(&["verify", path(&db2)]);

    let export2 = temp.path().join("rt.export2.cdb");
    run(&["export", path(&db2), "--branch", "main", "--out", path(&export2)]);

    assert_eq!(
        root_hash(&db),
        root_hash(&db2),
        "import->export->import is a fixpoint"
    );
    assert_eq!(
        std::fs::read_to_string(&export1).unwrap(),
        std::fs::read_to_string(&export2).unwrap(),
        "the generic projection is byte-stable"
    );

    // The projection shows the parametric forms.
    let projection = std::fs::read_to_string(&export1).unwrap();
    assert!(projection.contains("enum Option<T>"), "{projection}");
    assert!(projection.contains("Option<i64>"), "{projection}");
    assert!(projection.contains("Option<bool>"), "{projection}");
}

#[test]
fn wrong_type_argument_count_is_rejected() {
    reject(
        "too_many_args",
        "enum Option<T> {\n  none: unit\n  some: T\n}\nfn f(o: Option<i64, bool>) -> i64 = 0\n",
        "expects 1 type args, got 2",
    );
}

#[test]
fn bare_generic_in_a_type_position_is_rejected() {
    reject(
        "missing_args",
        "record Pair<T> {\n  first: T\n  second: T\n}\nfn f(p: Pair) -> i64 = 0\n",
        "expects 1 type args, got 0",
    );
}

const GENERIC_FN_IDENTITY: &str = r#"
fn id<T>(x: T) -> T = x

fn main() -> i64 =
  let a: i64 = id(40) in
  let b: bool = id(true) in
  if b then a + 2 else 0
"#;

#[test]
fn generic_function_identity_compiles_at_two_types() {
    // The generic-functions acceptance fixture: one generic function `id<T>`
    // monomorphized natively at two instantiations (`id<i64>`, `id<bool>`),
    // eval == native. Each instantiation lowers to a distinct native symbol.
    check_native("generic_fn_id", GENERIC_FN_IDENTITY, 42);
}

const GENERIC_FN_OVER_ENUM: &str = r#"
enum Option<T> {
  none: unit
  some: T
}

fn id<T>(x: T) -> T = x

fn unwrap_or<T>(o: Option<T>, d: T) -> T =
  case o of
    none(u) => d
  | some(x) => x

fn main() -> i64 =
  let a: i64 = id(40) in
  let b: bool = id(true) in
  let c: i64 = unwrap_or(Option::some(2), 0) in
  let d: i64 = unwrap_or(Option::none, 100) in
  if b then a + c + d else 0
"#;

#[test]
fn generic_function_over_a_generic_enum_compiles_natively() {
    // A generic function whose parameter is itself a generic type
    // (`unwrap_or<T>(Option<T>, T)`), called at `i64`. `Option::some(2)` infers
    // `T` from its payload; `Option::none` is anchored to `T` solved from the
    // `d` argument (the deferred-argument retry). `id<T>` is exercised at two
    // types in the same program.
    check_native("generic_fn_enum", GENERIC_FN_OVER_ENUM, 142);
}

const GENERIC_FN_OVER_RECORD: &str = r#"
record Pair<T> {
  first: T
  second: T
}
fn make<T>(a: T, b: T) -> Pair<T> = { first: a, second: b }
fn first_of<T>(p: Pair<T>) -> T = p.first
fn main() -> i64 = first_of(make(42, 7))
"#;

#[test]
fn generic_function_over_a_generic_record_compiles_natively() {
    // A generic function returning a generic type (`make<T> -> Pair<T>`) feeding
    // a generic function consuming one (`first_of<T>(Pair<T>) -> T`): a generic
    // call whose argument is another generic call, monomorphized transitively.
    check_native("generic_fn_record", GENERIC_FN_OVER_RECORD, 42);
}

const GENERIC_FN_DISTINCT_LAYOUTS: &str = r#"
record Wrap<T> {
  value: T
  tag: i64
}
fn tag_of<T>(w: Wrap<T>) -> i64 = w.tag
fn main() -> i64 =
  let a: Wrap<i64> = { value: 5, tag: 10 } in
  let b: Wrap<bool> = { value: true, tag: 100 } in
  tag_of(a) + tag_of(b)
"#;

#[test]
fn generic_function_distinct_monomorphizations_have_distinct_layouts() {
    // `tag_of<i64>` (over a 16-byte `Wrap<i64>`) and `tag_of<bool>` (over a
    // differently-laid-out `Wrap<bool>`) are distinct monomorphizations reading
    // the `tag` field at instantiation-specific offsets; both run natively, so a
    // shared instance would mis-read one of them.
    check_native("generic_fn_layouts", GENERIC_FN_DISTINCT_LAYOUTS, 110);
}

#[test]
fn generic_function_blames_to_its_type_params() {
    // Provenance: a generic function's birth is the `create_function` migration
    // that records its type parameters, so blame on the generic identifies the
    // parameters its instances derive from (an instance's symbol is the content
    // hash of a descriptor naming the generic).
    let temp = tempdir().unwrap();
    let db = temp.path().join("blamefn.sqlite");
    let src = temp.path().join("blamefn.cdb");
    std::fs::write(&src, GENERIC_FN_IDENTITY).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let blame = parse_json(&run(&["blame-symbol", path(&db), "id", "--json"]));
    let birth = &blame["birth_migration"];
    assert_eq!(
        birth["operation_kind"], "create_function",
        "generic function born by create_function: {blame}"
    );
    assert_eq!(
        birth["operation"]["type_params"],
        serde_json::json!(["T"]),
        "the birth migration records the generic function's type parameters: {blame}"
    );
}

#[test]
fn generic_function_program_round_trips_to_a_fixpoint() {
    // import -> export -> import is a fixpoint and the export is byte-stable for
    // a generic-function program: the projection emits `fn id<T>` /
    // `fn unwrap_or<T>` and bare generic calls (never the unnamed monomorphic
    // instances), and re-import reproduces the same instances and root hash.
    let temp = tempdir().unwrap();
    let db = temp.path().join("rtfn.sqlite");
    let src = temp.path().join("rtfn.cdb");
    std::fs::write(&src, GENERIC_FN_OVER_ENUM).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let export1 = temp.path().join("rtfn.export1.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export1)]);

    let db2 = temp.path().join("rtfn2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export1)]);
    run(&["verify", path(&db2)]);

    let export2 = temp.path().join("rtfn.export2.cdb");
    run(&["export", path(&db2), "--branch", "main", "--out", path(&export2)]);

    assert_eq!(
        root_hash(&db),
        root_hash(&db2),
        "import->export->import is a fixpoint for a generic-function program"
    );
    assert_eq!(
        std::fs::read_to_string(&export1).unwrap(),
        std::fs::read_to_string(&export2).unwrap(),
        "the generic-function projection is byte-stable"
    );
    let projection = std::fs::read_to_string(&export1).unwrap();
    assert!(projection.contains("fn id<T>"), "{projection}");
    assert!(projection.contains("fn unwrap_or<T>"), "{projection}");
    assert!(
        !projection.contains("MonomorphicFunctionInstance"),
        "instances must not be projected: {projection}"
    );
}

fn object_hash(kind: &str, canonical_payload: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codedb/object/v1\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(b"1");
    hasher.update(b"\0");
    hasher.update(canonical_payload.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn canonical_json(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => serde_json::to_string(value).expect("string"),
        JsonValue::Array(values) => format!(
            "[{}]",
            values.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        JsonValue::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(k, v)| format!(
                        "{}:{}",
                        serde_json::to_string(k).expect("key"),
                        canonical_json(v)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

#[test]
fn verify_rejects_an_instance_inconsistent_with_its_generic() {
    // `verify` recomputes each monomorphic instance's signature from its generic
    // (R11) and rejects one that does not derive from it. Construct such a root
    // by giving `tag_of<i64>`'s entry the *other* instance's (`tag_of<bool>`)
    // definition and signature: the entry stays internally consistent (so
    // `type_check_root` accepts it — the body re-types under either `Wrap<T>`),
    // but its symbol's descriptor still says `[i64]` while its signature is now
    // the `bool` instantiation's — the inconsistency only the generic-instance
    // check catches. `verify` scans every stored root, so the planted root is
    // validated and rejected.
    let temp = tempdir().unwrap();
    let db = temp.path().join("badinst.sqlite");
    let src = temp.path().join("badinst.cdb");
    std::fs::write(&src, GENERIC_FN_DISTINCT_LAYOUTS).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let root_hash = root_hash(&db);
    let conn = Connection::open(&db).unwrap();
    let root_json: String = conn
        .query_row(
            "SELECT payload_json FROM objects WHERE hash = ?1",
            [&root_hash],
            |row| row.get(0),
        )
        .unwrap();
    let mut root: JsonValue = serde_json::from_str(&root_json).unwrap();

    // The two `tag_of` instances are the symbols whose objects are descriptors.
    let instances: Vec<usize> = root["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            let symbol = entry["symbol"].as_str().unwrap();
            let kind: Option<String> = conn
                .query_row(
                    "SELECT kind FROM objects WHERE hash = ?1",
                    [symbol],
                    |row| row.get(0),
                )
                .ok();
            kind.as_deref() == Some("MonomorphicFunctionInstance")
        })
        .map(|(idx, _)| idx)
        .collect();
    assert_eq!(instances.len(), 2, "expected two tag_of instances");

    // Give instance[0] instance[1]'s definition+signature (keeping its symbol).
    let other_def = root["symbols"][instances[1]]["definition"].clone();
    let other_sig = root["symbols"][instances[1]]["signature"].clone();
    root["symbols"][instances[0]]["definition"] = other_def;
    root["symbols"][instances[0]]["signature"] = other_sig;

    let canonical = canonical_json(&root);
    let hash = object_hash("ProgramRoot", &canonical);
    conn.execute(
        "INSERT INTO objects (hash, kind, schema_version, payload_json, payload_size_bytes)
         VALUES (?1, 'ProgramRoot', 1, ?2, ?3)",
        (&hash, &canonical, canonical.len() as i64),
    )
    .unwrap();
    drop(conn);

    let stderr = run_fail(&["verify", path(&db)]);
    assert!(
        stderr.contains("bad_generic_instance"),
        "expected bad_generic_instance, got: {stderr}"
    );
}

#[test]
fn un_inferrable_generic_function_call_is_rejected() {
    // A generic function whose only `T`-bearing argument is a bare aggregate
    // literal (which needs `T` to type) leaves the type arguments
    // under-determined; this fails closed rather than guessing.
    reject(
        "uninferrable_generic_fn",
        "record Wrap<T> {\n  value: T\n  tag: i64\n}\nfn tag_of<T>(w: Wrap<T>) -> i64 = w.tag\nfn main() -> i64 = tag_of({ value: 5, tag: 10 })\n",
        "cannot infer type argument",
    );
}

#[test]
fn applying_a_type_parameter_is_rejected() {
    // Constraint-free generics are not higher-kinded: a type parameter may not take
    // arguments.
    reject(
        "higher_kinded",
        "record Bad<T> {\n  x: T<i64>\n}\n",
        "cannot take type or region arguments",
    );
}

// Recursive and mutually-recursive generic functions (R11, PLAN_V3 Phase 14): the
// remaining Phase 14 follow-on. A recursive generic function forms a *generic
// recursion group* — the clique binds its members' generic signatures (`<T>`)
// before any body is type-checked, so a member may call itself and its peers
// generically; the concrete instances are monomorphized at the lowering seam, the
// worklist co-materializing a mutually-recursive instance pair and terminating on
// the back-edge. The generic templates are never lowered, never projected, and the
// instances are unnamed derived symbols recursive by-symbol — so lowering, verify,
// reachability, and the import->export->import fixpoint all carry over unchanged.

const RECURSIVE_GENERIC_PICK: &str = r#"
fn pick<T>(a: T, b: T, n: i64) -> T =
  if n <= 0 then a else pick(b, a, n - 1)

fn main() -> i64 =
  let x: i64 = pick(10, 20, 3) in
  let f: bool = pick(true, false, 1) in
  if f then 0 else x
"#;

#[test]
fn recursive_generic_function_compiles_at_two_instantiations() {
    // The acceptance fixture: a SELF-recursive generic function `pick<T>` threading
    // and returning two `T` values through the recursion, monomorphized natively at
    // two instantiations (`pick<i64>`, `pick<bool>`), eval == native. The recursion
    // threads `T` (the args swap each call) and the result depends on BOTH instances
    // (`pick<bool>` must return `false` for the `i64` branch to be taken), so a
    // shared/erased instance would miscompute.
    check_native("recursive_generic_pick", RECURSIVE_GENERIC_PICK, 20);
}

const MUTUAL_GENERIC_STEPS: &str = r#"
fn even_steps<T>(x: T, n: i64) -> i64 = if n <= 0 then 100 else odd_steps(x, n - 1)
fn odd_steps<T>(x: T, n: i64) -> i64 = if n <= 0 then 200 else even_steps(x, n - 1)
fn main() -> i64 = even_steps(true, 3) + even_steps(7, 2)
"#;

#[test]
fn mutually_recursive_generic_functions_compile_natively() {
    // A mutually-recursive generic clique `{even_steps<T>, odd_steps<T>}` — each
    // member calls its peer generically. Instantiated at BOTH `bool` and `i64`
    // (four instances total: even/odd x bool/i64), so the monomorphization worklist
    // co-materializes each mutually-recursive pair, terminating on the back-edge.
    // even_steps(true,3) = 200 (3 hops to odd's base), even_steps(7,2) = 100; eval
    // == native == 300.
    check_native("mutual_generic_steps", MUTUAL_GENERIC_STEPS, 300);
}

const RECURSIVE_GENERIC_OVER_GENERIC_TYPE: &str = r#"
record Wrap<T> {
  item: T
  count: i64
}
fn bump<T>(w: Wrap<T>, n: i64) -> i64 =
  if n <= 0 then w.count else 1 + bump(w, n - 1)
fn main() -> i64 =
  let a: Wrap<i64> = { item: 5, count: 10 } in
  let b: Wrap<bool> = { item: true, count: 100 } in
  bump(a, 3) + bump(b, 2)
"#;

#[test]
fn recursive_generic_function_over_a_generic_type_compiles_natively() {
    // A self-recursive generic function whose parameter is itself a generic type
    // (`bump<T>(Wrap<T>, i64)`), threading the generic-typed `w: Wrap<T>` through the
    // recursion. `bump<i64>` (over a 16-byte `Wrap<i64>`) and `bump<bool>` (over a
    // differently-laid-out `Wrap<bool>`) are distinct monomorphizations reading
    // `w.count` at instantiation-specific offsets; both run natively (a shared
    // instance would mis-read one). bump(a,3)=13, bump(b,2)=102; eval == native.
    check_native(
        "recursive_generic_over_type",
        RECURSIVE_GENERIC_OVER_GENERIC_TYPE,
        115,
    );
}

#[test]
fn recursive_generic_function_blames_to_its_type_params() {
    // Provenance: a recursive generic function is born by the `create_recursion_group`
    // migration that binds the clique, and that migration records each member's type
    // parameters — so blame on the generic identifies the parameters its instances
    // derive from, even though it is part of a recursion group.
    let temp = tempdir().unwrap();
    let db = temp.path().join("blamerec.sqlite");
    let src = temp.path().join("blamerec.cdb");
    std::fs::write(&src, RECURSIVE_GENERIC_PICK).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let blame = parse_json(&run(&["blame-symbol", path(&db), "pick", "--json"]));
    let birth = &blame["birth_migration"];
    assert_eq!(
        birth["operation_kind"], "create_recursion_group",
        "a recursive generic function is born by create_recursion_group: {blame}"
    );
    let member = birth["operation"]["members"]
        .as_array()
        .expect("recursion group members")
        .iter()
        .find(|member| member["name"] == "pick")
        .expect("member pick");
    assert_eq!(
        member["type_params"],
        serde_json::json!(["T"]),
        "the recursion-group member records its type parameters: {blame}"
    );
}

#[test]
fn recursive_generic_function_program_round_trips_to_a_fixpoint() {
    // import -> export -> import is a fixpoint and the export is byte-stable for a
    // recursive-generic-function program: the member projects as `fn pick<T>` (a
    // recursion-group member round-trips as an ordinary generic `fn`), the bare
    // recursive call re-infers identically, and the unnamed monomorphic instances are
    // never projected — so re-import reproduces the same instances and root hash.
    let temp = tempdir().unwrap();
    let db = temp.path().join("rtrec.sqlite");
    let src = temp.path().join("rtrec.cdb");
    std::fs::write(&src, RECURSIVE_GENERIC_PICK).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let export1 = temp.path().join("rtrec.export1.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export1)]);

    let db2 = temp.path().join("rtrec2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export1)]);
    run(&["verify", path(&db2)]);

    let export2 = temp.path().join("rtrec.export2.cdb");
    run(&["export", path(&db2), "--branch", "main", "--out", path(&export2)]);

    assert_eq!(
        root_hash(&db),
        root_hash(&db2),
        "import->export->import is a fixpoint for a recursive-generic-function program"
    );
    assert_eq!(
        std::fs::read_to_string(&export1).unwrap(),
        std::fs::read_to_string(&export2).unwrap(),
        "the recursive-generic-function projection is byte-stable"
    );
    let projection = std::fs::read_to_string(&export1).unwrap();
    assert!(projection.contains("fn pick<T>"), "{projection}");
    assert!(
        !projection.contains("MonomorphicFunctionInstance"),
        "instances must not be projected: {projection}"
    );
}

#[test]
fn mutually_recursive_generic_program_round_trips_to_a_fixpoint() {
    // A mutually-recursive generic clique must round-trip to a fixpoint. The
    // projection orders a callee before its caller by *named* dependency, and a
    // generic clique member calls its peers at `TypeParam` arguments (whose unnamed
    // instance does not exist) — so the projection ordering must follow the named
    // peer, not the absent instance, or `main` would be emitted between the two
    // clique members, shift its parse index, and re-identify it (breaking the root
    // hash even though the projection text is byte-stable). This pins both halves.
    let temp = tempdir().unwrap();
    let db = temp.path().join("rtmut.sqlite");
    let src = temp.path().join("rtmut.cdb");
    std::fs::write(&src, MUTUAL_GENERIC_STEPS).unwrap();
    run(&["init", path(&db)]);
    run(&["import", path(&db), path(&src)]);

    let export1 = temp.path().join("rtmut.export1.cdb");
    run(&["export", path(&db), "--branch", "main", "--out", path(&export1)]);

    let db2 = temp.path().join("rtmut2.sqlite");
    run(&["init", path(&db2)]);
    run(&["import", path(&db2), path(&export1)]);
    run(&["verify", path(&db2)]);

    let export2 = temp.path().join("rtmut.export2.cdb");
    run(&["export", path(&db2), "--branch", "main", "--out", path(&export2)]);

    assert_eq!(
        std::fs::read_to_string(&export1).unwrap(),
        std::fs::read_to_string(&export2).unwrap(),
        "the mutually-recursive-generic projection is byte-stable"
    );
    assert_eq!(
        root_hash(&db),
        root_hash(&db2),
        "import->export->import is a fixpoint for a mutually-recursive-generic clique"
    );
    let projection = std::fs::read_to_string(&export1).unwrap();
    assert!(projection.contains("fn even_steps<T>"), "{projection}");
    assert!(projection.contains("fn odd_steps<T>"), "{projection}");
}
