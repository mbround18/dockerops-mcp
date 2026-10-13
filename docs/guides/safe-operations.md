# Safe operations

Safety controls are part of the protocol contract, not optional UX.

## Confirmation gates

These operations refuse without `confirm: true`:

- `compose` verbs: `down`, `rm`, `kill`
- `passthrough` commands classified as `destroy` (for example `rm`, `rmi`, `prune`, `volume rm`, `network rm`)

If `volumes: true` is requested on compose removal paths, refusal messaging explicitly warns about data deletion.

## Read-only mode

Start the server with:

```bash
dockerops-mcp --read-only
```

Effects:

- `compose`, `run`, and `exec` refuse
- `passthrough` permits only commands classified as `read`
- dedicated read tools (`ps`, `logs`, `inspect`, `stats`, `images`, `usage`, `config`, `events`) remain available

## Bounded execution guarantees

Calls that can hang are refused or normalized:

- `compose up` always detached
- `run` requires detached (`-d`) or ephemeral (`--rm`) mode
- `events` always includes `--until`
- `stats` always non-streaming
- passthrough refuses `attach`, `wait`, `compose watch`, `logs -f`, `events` without `--until`, `stats` without `--no-stream`, and `compose up` without `-d`

Interactive and terminal flags are allowed on `run`, `create`, `exec` and `start`:

- `-i` keeps stdin open; pass the input as the `stdin` parameter (up to 1 MiB). Without it stdin is empty, so the command reads end-of-file instead of waiting.
- `-t` allocates a terminal; carriage returns and escape codes are stripped from the output.
- `-it` runs as `-i`, because stdin is never a terminal and docker would refuse the pair. The footer says `-t` was dropped.

Elsewhere `-i` and `-t` are left alone: `build -t` is a tag, `logs -t` timestamps, `load -i` an input file, and anything after the image or container belongs to the command being run.

## Classification model for passthrough

`passthrough` classifies subcommands as:

- `read`
- `mutate`
- `destroy`

Unknown verbs default to `mutate` (conservative). This protects read-only and avoids accidental unsafe assumptions.
