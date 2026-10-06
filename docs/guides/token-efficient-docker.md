# Token-efficient Docker workflows

The server exists to replace common CLI patterns that spend tokens on intermediate text.

## Replace pipelines with tool parameters

| Typical shell flow | MCP equivalent |
|---|---|
| `docker logs api 2>&1 | grep -i error | tail -40` | `logs` with `targets`, `level`/`grep`, `tail` |
| `docker inspect api | jq '.State.Health'` | `inspect` with `preset:"health"` or `fields` |
| `docker exec c cmd | grep -v noisy` | `exec` with `exclude` |
| `docker compose config` then scan YAML | `config` with `mode` |
| `cd /repo && docker compose config --quiet` | `config` with `cwd`, `mode:"validate"` |
| `cd /repo && docker compose config --services` | `config` with `cwd`, `mode:"services"` |
| `cd /repo && docker build --target wine --tag enshrouded-wine-test .` | `passthrough` with `cwd`, `args:["build","--target","wine","--tag","enshrouded-wine-test","."]` |
| Long shell loop for cold-volume startup checks across images | `verify_startup` with `images`, `probe_url`, `env`, `publish`, `volume_target` |
| `docker events ...` in terminal then Ctrl+C | `events` with `since/until` window |

## Batch first

One call with multiple targets costs less than repeated single-target calls.

Example:

```json
{
  "tool":"logs",
  "arguments":{
    "targets":["api","worker","scheduler"],
    "level":"error",
    "since":"20m",
    "tail":150
  }
}
```

## Prefer bounded windows

- Use `since` + `until` where available.
- Use `tail` + `max_lines` to keep responses focused.
- Use `max_bytes` only when you need tighter budget control than defaults.

## Extract only needed structure

- `inspect.fields`: pinpoint values by dotted paths.
- `config.mode`: choose `services`, `images`, `resources`, or `profiles`.
- `events.collapse`: condense repeated crash-loop events.

## When to use passthrough

Use `passthrough` only for docker/compose commands not covered by dedicated tools. It still enforces:

- bounded execution rules
- classify-and-confirm safety gates
- response line and byte limits
