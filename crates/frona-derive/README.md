# frona-derive

Procedural macros used by Frona libraries and the Frona application.

## Annotated harness tools

Applications embedding the harness should depend on `frona-harness = "0.1"` and
import `frona_harness::tool`. The harness re-exports this crate's attribute macro
and supplies the runtime types that generated tools use.

Annotate an async function with `#[tool]`, describe it with a Rust doc comment,
and register it with `.tool(function_name)`. Parameters become model arguments;
a `ToolContext` parameter is supplied by the harness and omitted from the schema.
The original function remains directly callable. See the
[harness guide](https://docs.rs/frona-harness) for complete, executable examples.

## Direct dependency

```toml
[dependencies]
frona-derive = "0.1"
```

A direct dependency does not provide the harness runtime. The other macros,
including `Entity`, `ChannelFactory`, and application tool registration, target
Frona's application interfaces and resource layout. They are not general-purpose
ORM or channel abstractions for unrelated applications. `ParameterMetadata`
generates protocol parameter metadata for the corresponding Frona interfaces.

There are no optional Cargo features. See the
[macro reference](https://docs.rs/frona-derive) for the available attributes.

## License

MIT. See `LICENSE`. Dependencies have their own licenses.
