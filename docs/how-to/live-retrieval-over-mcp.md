# Serve your index to AI agents over MCP

**Goal:** instead of exporting a static file, let an AI client query your live index on demand
through the [Model Context Protocol](https://modelcontextprotocol.io). The agent browses the tree,
searches, reads files, and asks grounded questions — pulling only the slice it needs, when it needs
it, so a small model's context window stays bounded.

## 1. Build an index first

MCP serves an existing index; it doesn't build one. If you haven't yet:

```bash
indexa index ~/code/myproject
indexa doctor              # confirm Ollama + models are ready
```

## 2. Run the server

The MCP server speaks JSON-RPC over **stdio** — clients launch it as a subprocess, so you usually
don't run it by hand. To smoke-test it:

```bash
indexa mcp        # waits on stdin/stdout; Ctrl-C to stop
```

## 3. Register it with a client

**Claude Desktop / Claude Code** — add to the MCP servers config:

```json
{
  "mcpServers": {
    "indexa": { "command": "indexa", "args": ["mcp"] }
  }
}
```

**Cursor** and other MCP clients use the same `command` + `args` shape. Use an absolute path to the
binary (e.g. `/usr/local/bin/indexa`) if `indexa` isn't on the client's `PATH`.

## 4. What the agent can do

The server exposes 33 tools. The ones you'll see used most:

| Tool | Purpose |
|---|---|
| `browse_tree` | list a directory's indexed children |
| `get_summary` (tier `l0`/`l1`/`l2`) | scan cheaply at `l0` (one line), drill to `l1` (full) or `l2` (raw) |
| `search` | hybrid keyword + semantic search across content |
| `read_file` | raw file text (confined to indexed roots) |
| `ask` | grounded RAG answer (supports `scope`, `mode`, `agentic`) |
| `dependencies` / `who_imports` / `who_calls` / `blast_radius` / `code_graph` | code-graph navigation (see [debugging the code graph](debug-the-code-graph.md)) |
| `create_pack` / `add_pack_paths` / `export_pack` | build and hand over Context Packs |

The progressive-disclosure pattern (`l0` → `l1` → `l2`) is the point: the agent surveys with
one-line abstracts and only spends tokens reading detail where it matters.

## Safety notes

- `read_file` is **confined to indexed roots** — an agent can't read `/etc/passwd` or escape via
  `../` through the tool.
- The server runs locally over stdio; it isn't a network service.
