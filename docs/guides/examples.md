# Examples

## 1) Crash loop diagnosis

```json
{
  "tool":"events",
  "arguments":{
    "targets":["enshrouded-docker-wine-1"],
    "types":["container"],
    "actions":["die","start","restart","health_status"],
    "since":"45m",
    "collapse":true
  }
}
```

Why: bounded event window, repeated events folded, no hanging stream.

## 2) Focused error logs across services

```json
{
  "tool":"logs",
  "arguments":{
    "project":"ens",
    "targets":["api","wine","worker"],
    "level":"error",
    "exclude":["healthcheck","heartbeat"],
    "tail":200,
    "max_lines":120
  }
}
```

Why: one batched call instead of three independent tails and greps.

## 3) Resolve compose facts instead of reading merged YAML

```json
{"tool":"config","arguments":{"files":["docker-compose.yml"],"mode":"resources"}}
```

Why: lists declared volumes/networks and service usage directly.

## 3b) Fast compose validity check (like `docker compose config --quiet`)

```json
{
  "tool":"config",
  "arguments":{
    "cwd":"/home/mbruno/development/docker/enshrouded-docker",
    "mode":"validate"
  }
}
```

Why: one bounded response that answers only valid/invalid without shipping merged YAML.

## 4) SteamCMD-style noisy exec output, server-side filtered

```json
{
  "tool":"exec",
  "arguments":{
    "target":"enshrouded-docker-wine-1",
    "argv":["steamcmd","+app_update","2278520","validate","+quit"],
    "grep":"error|failed",
    "exclude":"^Update state",
    "ignore_case":true,
    "max_lines":80
  }
}
```

Why: suppresses progress spam before transport; preserves failing line + exit status.

## 5) One-off container run (bounded and ephemeral)

```json
{"tool":"run","arguments":{"image":"hello-world"}}
```

Why: dedicated `run` tool auto-adds ephemeral behavior by default (`--rm`) and keeps line/byte limits.

## 5b) Cold-volume startup verification across Proton and Wine images

```json
{
  "tool":"verify_startup",
  "arguments":{
    "cwd":"/home/mbruno/development/docker/enshrouded-docker",
    "images":["enshrouded-proton-test","enshrouded-wine-test"],
    "env":[
      "NAME=Verify Test",
      "SET_GROUP_ADMIN_PASSWORD=change-me",
      "SET_GROUP_GUEST_PASSWORD=change-me-too",
      "HTTP_PORT=3000",
      "RUST_LOG=info"
    ],
    "publish":["3000:3000"],
    "volume_target":"/home/steam/enshrouded",
    "probe_url":"http://127.0.0.1:3000/ready",
    "initial_delay_seconds":30,
    "interval_seconds":10,
    "max_attempts":120,
    "log_tail_lines":80,
    "cleanup":true
  }
}
```

Why: replaces a long shell loop (`docker run` + `curl` + `docker ps` + `docker logs` + cleanup) with one bounded call and per-image evidence.

## 6) Safe destructive compose action

Refusal example (missing confirmation):

```json
{"tool":"compose","arguments":{"verb":"down","project":"ens"}}
```

Allowed explicitly:

```json
{"tool":"compose","arguments":{"verb":"down","project":"ens","confirm":true}}
```

## 7) Long-tail CLI command with passthrough

```json
{
  "tool":"passthrough",
  "arguments":{
    "args":["network","inspect","ens_default"],
    "max_lines":120
  }
}
```

Docker build example:

```json
{
  "tool":"passthrough",
  "arguments":{
    "cwd":"/home/mbruno/development/docker/enshrouded-docker",
    "args":["build","--target","wine","--tag","enshrouded-wine-test","."]
  }
}
```

Feeding input to a container with `-i`:

```json
{
  "tool":"passthrough",
  "arguments":{
    "args":["exec","-i","db","psql","-U","app"],
    "stdin":"select count(*) from users;\n"
  }
}
```

Refused unbounded example:

```json
{"tool":"passthrough","arguments":{"args":["logs","-f","api"]}}
```
