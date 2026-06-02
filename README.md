# codedb

`codedb` is a Rust proof of concept for the model in [docs/SPEC.md](docs/SPEC.md): programs are stored as immutable, content-addressed semantic objects in SQLite, and source files are projections. `emit-c` emits a C projection for debugging and inspection; it is not the primary compiler backend.

## Demo

```bash
cargo run -- init demo.codedb.sqlite
cargo run -- import demo.codedb.sqlite examples/shop.cdb
cargo run -- apply demo.codedb.sqlite --json operations.json
cargo run -- eval demo.codedb.sqlite main
cargo run -- callers demo.codedb.sqlite tax
cargo run -- show demo.codedb.sqlite tax
cargo run -- set-export demo.codedb.sqlite tax public_tax
cargo run -- export-map demo.codedb.sqlite
cargo run -- rename demo.codedb.sqlite tax vat
cargo run -- export demo.codedb.sqlite --branch main --out projection.cdb
cargo run -- emit-c demo.codedb.sqlite main --out projection.c
cargo run -- replay demo.codedb.sqlite --from-genesis
cargo run -- verify demo.codedb.sqlite
```

Expected `eval main` result for `examples/shop.cdb` is `120`. After `rename tax vat`, the exported projection renders `vat` at both the definition and call site while preserving the original symbol hash and function body hash.

Native ABI identity is separate from display names. `show` prints the stable internal ABI symbol derived from the symbol hash, while `set-export` and `remove-export` manage explicit public ABI names. Renaming `tax` to `vat` does not change either the internal ABI symbol or an explicit export such as `public_tax`.

Structural mutation commands return `applied`, `already_applied`, or `conflict`.
Use `--expect-root <root>` on mutation commands when an agent needs to reject
stale writes instead of applying against a branch that has moved.

`codedb apply <db> --json <file>` accepts a structural `codedb/apply/v1` JSON
document with operations such as `create_function`, `rename_symbol`,
`replace_function_body`, `create_alias`, and `set_export`. Function bodies are
structural expression JSON objects, so agents do not need to write projection
text to mutate the database.
