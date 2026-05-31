# codedb

`codedb` is a Rust proof of concept for the model in [SPEC.md](SPEC.md): programs are stored as immutable, content-addressed semantic objects in SQLite, and source files are projections.

## Demo

```bash
cargo run -- init demo.codedb.sqlite
cargo run -- import demo.codedb.sqlite examples/shop.cdb
cargo run -- eval demo.codedb.sqlite main
cargo run -- callers demo.codedb.sqlite tax
cargo run -- rename demo.codedb.sqlite tax vat
cargo run -- export demo.codedb.sqlite --branch main --out projection.cdb
cargo run -- emit-c demo.codedb.sqlite main --out projection.c
cargo run -- replay demo.codedb.sqlite --from-genesis
cargo run -- verify demo.codedb.sqlite
```

Expected `eval main` result for `examples/shop.cdb` is `120`. After `rename tax vat`, the exported projection renders `vat` at both the definition and call site while preserving the original symbol hash and function body hash.
