# CLI output schemas

One JSON Schema per `bee` command (`<command>.schema.json`), plus `error.schema.json`.
Every document carries a `schema` field such as `bee.v1.status`. Schemas are versioned with
the API: fields may be added, never removed or retyped. Plan task W12 writes the rest.

## Message schemas

`messages/<message>.schema.json` holds one self-contained JSON Schema (draft 2020-12) for each
RPC request and response message, in the proto3-JSON mapping (SPEC §12.1). MCP tools use them as
input and output schemas. They are generated from `proto/bee/v1`, together with
`schemas/openapi/bee.v1.json`; do not edit them by hand. Regenerate with:

```sh
cargo run -p bee-proto --bin bee-proto-gen
```

`cargo test -p bee-proto` fails when they are stale.
