# Add an MCP tool (contributor guide)

**Goal:** expose a new capability to AI clients (Claude Desktop, Cursor, …) over the
[Model Context Protocol](https://modelcontextprotocol.io) server (`indexa mcp`).

> This is a **contributor** guide, not a user recipe. For *using* the server see
> [Serve your index to AI agents over MCP](live-retrieval-over-mcp.md).

## Where tools live

Tools are **not** defined in `crates/mcp/src/lib.rs` — that file only composes the
routers and hosts the server. Each tool is a method on `IndexaMcp`, grouped by family
into a **router module** under `crates/mcp/src/`:

| Module | Family |
|---|---|
| `retrieval.rs` | search / browse / summary / read / ask |
| `graph.rs` | code-graph (`dependencies`, `who_calls`, `blast_radius`, …) |
| `curation.rs` | importance weights + classification |
| `packs.rs` | Context Packs |
| `review.rs` | the Decision Ledger |
| `insights.rs` | duplicates / stale / languages / largest / diff |
| `admin.rs` | stats / config / prune / trigger-index / formats |
| `query_extras.rs` | `project_overview`, `explain_retrieval`, `inspect` |

Pick the module whose family fits. `query_extras.rs` is the smallest and the best one
to read end-to-end before you start.

## Steps

### 1. Add the tool method

Inside the module's `#[tool_router(...)] impl IndexaMcp { … }` block, add an `async`
method annotated with `#[tool(description = …)]`. Agents pick tools **by the
description**, so make it specific and action-oriented. Open the index with
`self.store()?` and return text with `ok_text`; convert errors with `.map_err(mcp_err)`.

A parameter-free tool:

```rust
/// One-line doc for humans reading the source.
#[tool(description = "What this does and when an agent should call it (agents choose by this text).")]
pub(crate) async fn my_tool(&self) -> Result<CallToolResult, ErrorData> {
    let store = self.store()?;
    let n = store.entry_count().map_err(mcp_err)?;
    Ok(ok_text(format!("{n} entries indexed.")))
}
```

A tool with arguments — declare a `Deserialize + schemars::JsonSchema` params struct
(its field doc-comments become the JSON-schema descriptions the client sees) and take
it as `Parameters<…>`:

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MyToolParams {
    /// Absolute path to inspect.
    pub path: String,
    /// Optional cap on results (default 10).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[tool(description = "…")]
pub(crate) async fn my_tool(
    &self,
    params: Parameters<MyToolParams>,
) -> Result<CallToolResult, ErrorData> {
    let MyToolParams { path, limit } = params.0;
    let store = self.store()?;
    // …
    Ok(ok_text("…"))
}
```

`project_overview` / `inspect` in [`query_extras.rs`](../../crates/mcp/src/query_extras.rs)
are working examples of both shapes.

### 2. (Router composition is automatic)

You do **not** edit `lib.rs` to register the method. Each module's
`#[tool_router(router = router_<name>, vis = "pub(crate)")]` collects its tools, and
`IndexaMcp::tool_router()` in `lib.rs` already sums every router with `+`. A method
added to an existing router module is wired up automatically.

(You only touch `lib.rs` if you create a *brand-new* router module — then add
`mod <name>;` and one `+ Self::router_<name>()` line to `tool_router()`.)

### 3. Update the golden list and the docs

The tool surface is a published API guarded by two contract tests in `lib.rs`:

- `tool_contract_golden_list` compares the live tool list against
  [`crates/mcp/golden_tools.txt`](../../crates/mcp/golden_tools.txt) and fails on any
  add / remove / rename.
- `doc_tool_count_matches_code` fails if the "N tools" count in `README.md`,
  `CLAUDE.md`, or `docs/how-to/live-retrieval-over-mcp.md` disagrees with the code.

So, after adding a tool:

```bash
INDEXA_UPDATE_GOLDEN=1 cargo test -p indexa-mcp   # regenerate golden_tools.txt
```

Commit the updated `golden_tools.txt`, then bump the tool count in `README.md`,
`CLAUDE.md`, and `docs/how-to/live-retrieval-over-mcp.md`.

### 4. Verify

```bash
cargo test -p indexa-mcp        # golden + doc-count + every_tool_has_a_description + golden-call tests
cargo clippy -p indexa-mcp -- -D warnings
```

`every_tool_has_a_description` will fail if you left the description empty — that field
is mandatory because it is how agents decide when to call your tool.

## Out of scope

MCP tools are **in-tree** methods on `IndexaMcp`, not out-of-process plugins — there is
no example binary to copy and no plugin manifest. (The Plugin SDK in `crates/parsers`
is a different extension point, for file-format parsers only.)
