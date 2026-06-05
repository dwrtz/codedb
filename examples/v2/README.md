# examples/v2 - Native Acceptance Programs

Status: v2 acceptance fixture index

V2 acceptance programs prove that CodeDB features compile from semantic objects
to native artifacts. These examples are native-required gates: the reference
evaluator may be used as an oracle, but evaluator-only success does not accept a
v2 feature.

The required acceptance programs are:

| Program | Purpose | Native-required gate |
| --- | --- | --- |
| `line_view_refs.cdb` | Shared references stored in records. | Records containing shared references compile natively and pass semantic tests. |
| `mutable_cursor.cdb` | Mutable references stored in records plus state effects. | Exclusive mutable access compiles to native stores and rejects aliasing. |
| `invoice_static.cdb` | Records, enums, fixed arrays or slices, references, and loops or folds. | Structured static data compiles natively with deterministic layouts. |
| `parser_or_word_count.cdb` | Byte or string slices, bounds checks, loops, and later I/O. | Slice-heavy parsing or counting compiles natively without interpreter fallback. |
| `todo_cli.cdb` | Capstone CLI with args, stdout, files, strings, dynamic allocation, and result handling. | Useful stateful CLI behavior passes native-required integration tests. |

Each acceptance program should eventually include:

- source projection fixture;
- structural apply JSON fixture where useful;
- native-required semantic tests;
- trace/debug fixture;
- verify fixture;
- replay/export/import fixture.

Implemented fixtures are added as their corresponding semantic objects,
verification, lowering, and native backends become available.
