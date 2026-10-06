# CLAUDE.md

This repository provides an MCP server for Docker/Compose operations optimized for token-constrained agent sessions.

## Preferred usage

- Use dedicated tools (`logs`, `inspect`, `events`, `config`, etc.) before falling back to `passthrough`.
- Ask for filtering in parameters (`grep`, `exclude`, `fields`, `level`) instead of shell pipelines.
- Batch targets in one call whenever possible.

## Safety model

- Destructive actions require `confirm: true`.
- `--read-only` blocks mutating tools (`compose`, `run`, `exec`, mutating/destroy passthrough commands).
- Streaming and attaching behavior is refused so calls always terminate.

## High-value examples

Check unhealthy containers in one call:

```json
{"tool":"ps","arguments":{"filter":["health=unhealthy"]}}
```

Show crash-loop evidence from daemon events:

```json
{
  "tool":"events",
  "arguments":{
    "targets":["enshrouded-docker-wine-1"],
    "actions":["die","restart","health_status"],
    "since":"30m",
    "collapse":true
  }
}
```

Summarize compose project facts (resolved config):

```json
{
  "tool":"config",
  "arguments":{
    "files":["docker-compose.yml"],
    "mode":"summary"
  }
}
```

Run bounded in-container diagnostic command:

```json
{
  "tool":"exec",
  "arguments":{
    "target":"enshrouded-docker-wine-1",
    "argv":["bash","-lc","ls -lah /home/steam"],
    "max_lines":80
  }
}
```

Run an ephemeral one-off container:

```json
{
  "tool":"run",
  "arguments":{
    "image":"hello-world"
  }
}
```

Use passthrough for non-curated subcommands:

```json
{
  "tool":"passthrough",
  "arguments":{
    "args":["network","inspect","ens_default"],
    "max_lines":120
  }
}
```

## Local dev commands

```bash
make build
make test
make lint
make install
```
