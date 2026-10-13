# Contributing to dockerops-mcp

This project optimizes for **high-signal Docker answers under strict token budgets**. Keep behavior predictable, bounded, and explicit.

## Development loop

```bash
make build
make test
make lint
make install
```

`make install` is required after code changes so registered MCP clients execute the latest binary.

## Core invariants (do not violate)

1. **Every tool call must terminate.**
   - No unbounded stream/follow/attach behavior.
   - `compose up` is always detached.
   - `events` always sets `--until`.
   - `stats` always uses `--no-stream`.
   - `passthrough` refuses endless patterns by name.

2. **Destructive actions require explicit confirmation.**
   - `compose down`, `compose rm`, `compose kill`, and destructive passthrough commands refuse without `confirm: true`.
   - Refusal text must state the impact and indicate a safe read tool to check state first.
   - `down` + `volumes: true` must call out data deletion risk explicitly.

3. **Read-only mode is a hard gate.**
   - In `--read-only`, mutation/destroy operations refuse.
   - `run`, `exec`, and `compose` must refuse in read-only mode.
   - Dedicated read tools remain fully functional.
   - Passthrough allows only commands classified as `read`.

4. **Filter before rendering.**
   - Use server-side `grep` / `exclude` / presets / field extraction.
   - Avoid shipping noise only to trim it afterward.
   - Keep line limits and byte budgets in effect.

5. **Response text and structured content must agree.**
   - `text` is the user-facing answer.
   - `structuredContent.text` must carry the same rendered string.
   - This guards host differences in MCP rendering.

6. **Per-target failures stay in-band.**
   - A partially successful batch should still succeed as a tool call.
   - A failed target is represented as one target's output, not a protocol-level failure.

7. **Request errors are readable tool errors.**
   - Reject invalid argument combinations early and clearly.
   - Return request feedback the caller can directly fix.

8. **No implicit shell execution.**
   - Tools accept argv lists by default.
   - `exec.shell` is explicit and opt-in for pipes/redirects.
   - passthrough stdin is a closed pipe (empty, or the caller's `stdin` text), so `-i`
     always reaches end-of-file. `-it` is rewritten to `-i`, and `-t` output has its
     escape codes stripped. Only the option span of `run`/`create`/`exec`/`start` is
     rewritten, never the container's own command.

9. **Unknown passthrough subcommands are conservative.**
   - Unrecognized verbs classify as `mutate`, never `read`.
   - This protects read-only and confirmation boundaries.

10. **Compose config parse failures are surfaced as output.**
    - A compose syntax error is usually the answer, not a hidden internal failure.

## Implementation guidance

- Keep tool schemas descriptive and aligned with behavior.
- Prefer narrow, composable helpers over ad hoc output parsing.
- Preserve existing style: concise rendered text + machine-readable structured fields.
- Avoid broad catches and silent fallbacks; refusals should explain what to do instead.

## Testing expectations

At minimum, run targeted tests for changed modules. Before merge, run full workspace checks:

```bash
make check
```

When altering:
- **boundedness rules** -> add/adjust refusal tests
- **classification/safety gates** -> add passthrough + compose confirmation tests
- **output formatting** -> assert exact rendered text and footer behavior
- **MCP server wiring** -> update e2e tests under `apps/dockerops-mcp/tests`

## Documentation updates

Behavioral changes must update:

- `README.md`
- `docs/guides/output-format.md` (if output shape changes)
- `docs/guides/safe-operations.md` (if guardrails change)
- `CLAUDE.md` examples (if call patterns change)
