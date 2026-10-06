# dockerops-mcp

Token-efficient Docker + Docker Compose operations over MCP stdio.

This server is built for agent workflows where raw `docker` output wastes context. It exposes twelve tools that return bounded, readable summaries with structured data attached.

## What you get

- **Bounded calls by design**: no streaming/follow mode, no hanging attach sessions.
- **Server-side filtering**: `grep`, `exclude`, `level`, `fields`, `max_lines`, `max_bytes`.
- **Safety rails**: destructive operations require `confirm: true`; read-only mode blocks mutations.
- **Consistent response shape**: rendered text + `structuredContent` carry the same answer.

## Tools

| Tool | Purpose |
|---|---|
| `ps` | One-line container/service status view. |
| `logs` | Bounded log windows with server-side filtering and collapse. |
| `inspect` | Dotted-path/preset extracts from inspect documents. |
| `stats` | Non-streaming resource snapshot, worst-first. |
| `images` | Disk-heavy images first, with totals. |
| `usage` | `docker system df` style usage + reclaimability. |
| `config` | Compose config resolved to actionable facts (not raw YAML). |
| `events` | Bounded daemon event window with optional collapse. |
| `compose` | Ten lifecycle verbs with summarized outcomes. |
| `run` | Bounded `docker run` for one-off and detached containers. |
| `exec` | One-shot container command with filtered output and exit status. |
| `passthrough` | Long tail of docker/compose subcommands under guardrails. |

## Install

```bash
make install
```

Register with Claude Code:

```bash
claude mcp add --scope user dockerops dockerops-mcp
```

Run read-only when needed:

```bash
dockerops-mcp --read-only
```

## Examples

Get only error lines from the last logs:

```json
{"tool":"logs","arguments":{"targets":["api"],"level":"error","tail":120}}
```

Inspect health quickly:

```json
{"tool":"inspect","arguments":{"targets":["api"],"preset":"health"}}
```

Reproduce an `exec | grep -v` pipeline server-side:

```json
{
  "tool":"exec",
  "arguments":{
    "target":"enshrouded-docker-wine-1",
    "argv":["steamcmd","+app_update","2278520","validate","+quit"],
    "grep":"error|failed",
    "exclude":"^Update state",
    "ignore_case":true
  }
}
```

Run `hello-world` as a one-off container (ephemeral by default):

```json
{"tool":"run","arguments":{"image":"hello-world"}}
```

Summarize compose resources:

```json
{"tool":"config","arguments":{"files":["docker-compose.yml"],"mode":"resources"}}
```

See `docs/guides/` for the full examples and invariants.
