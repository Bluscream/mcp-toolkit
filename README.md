# mcp-toolkit

Shared machinery for a family of standalone MCP tool servers.

Each server (`hex-mcp`, `ssh-mcp`, `eval-mcp`, …) is a thin crate: implement
`ToolGroup`, call `run`. Everything else lives here, so a fix lands in every
server at once:

- **CLI** — `--transport`, `--single-tool`, `--timeout`, `--bind`,
  `--auth-token`, `--list-tools`, all with env-var equivalents.
- **Transports** — stdio and MCP Streamable HTTP, via the official
  [`rmcp`](https://crates.io/crates/rmcp) SDK.
- **`--single-tool`** — collapses every tool into one dispatching tool for
  clients that cap how many they accept.
- **Timeouts** — a `timeout` argument on every tool, clamped to `--max-timeout`.
- **Auth** — bearer token on the HTTP transports; refuses to start without one
  unless `--allow-unauthenticated` is passed.

## Usage

```rust
use mcp_toolkit::{ServerOptions, ToolDef, ToolGroup, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};

struct Greeter;

#[async_trait]
impl ToolGroup for Greeter {
    fn tools(&self) -> Vec<ToolDef> {
        vec![ToolDef::new("greet", "Say hello.", json!({ "type": "object" }))]
    }
    async fn call(&self, _name: &str, _args: Value) -> ToolResult<ToolOutput> {
        Ok(ToolOutput::text("hello"))
    }
}
```

```rust,ignore
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = ServerOptions::parse();
    mcp_toolkit::run("greeter", env!("CARGO_PKG_VERSION"), Arc::new(Greeter), options).await?;
    Ok(())
}
```

## Why a `ToolGroup` trait rather than rmcp's macros

`ToolGroup` is plain data plus an async function, deliberately independent of
`rmcp`. That means a tool crate can be unit-tested without standing up a
protocol server, and a host process can embed the tools directly — which is how
omni-mcp consumes them, as library calls rather than sidecar subprocesses.

`--single-tool` also needs to synthesise a tool at runtime from the others,
which the attribute macros cannot express.

## Notes

`--transport sse` is accepted as an alias for `http`. The standalone SSE
transport was deprecated in the MCP specification in favour of Streamable HTTP,
which uses SSE for its streaming responses.

## Development

```bash
./scripts/build.sh
```

Runs `cargo fmt --check`, `cargo clippy -D warnings` (max 100 lines per
function) and the tests (which enforce max 1000 lines per file).

## License

[Unlicense](LICENSE) (public domain).
