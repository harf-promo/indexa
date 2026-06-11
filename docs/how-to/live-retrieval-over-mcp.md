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

In every client the registration is the same idea — *launch `indexa` with the arg `mcp` over
stdio* — only the config file differs. Use an absolute path to the binary (`which indexa`) if it
isn't on the client's `PATH`.

**Claude Code** — one command, no file editing:

```bash
claude mcp add indexa -- indexa mcp        # current project
claude mcp add --scope user indexa -- indexa mcp   # all your projects
```

(Or check a `.mcp.json` into the repo to share it with collaborators:
`{"mcpServers": {"indexa": {"command": "indexa", "args": ["mcp"]}}}`.)

**Claude Desktop** — add to `claude_desktop_config.json`
(macOS: `~/Library/Application Support/Claude/`, Windows: `%APPDATA%\Claude\`):

```json
{
  "mcpServers": {
    "indexa": { "command": "indexa", "args": ["mcp"] }
  }
}
```

**Cursor** — same JSON shape in `~/.cursor/mcp.json` (all projects) or `.cursor/mcp.json`
(one project).

**VS Code (Copilot agent mode)** — `.vscode/mcp.json` in the workspace, note the `servers` key:

```json
{
  "servers": {
    "indexa": { "command": "indexa", "args": ["mcp"] }
  }
}
```

To verify any of them: ask the agent *"using indexa, what's in `<some indexed folder>`?"* — you
should see a `browse_tree` or `search` call.

## 4. What the agent can do

The server exposes 34 tools. The ones you'll see used most:

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
