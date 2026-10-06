# Output format

Each tool returns:

1. Rendered human-readable **text**
2. Structured machine-readable fields in **structuredContent**

The rendered text is duplicated in `structuredContent.text` for host compatibility.

## Text conventions

- Primary rows first (container/service/event lines)
- Explicit footer summary in brackets
- Truncation/filter counts called out in the footer
- Exit status included for command-style tools (`exec`, `passthrough`, `compose` failure)

## Footer patterns

Examples:

- `[3 targets, 12 lines, 8 filtered out]`
- `[6 events in 30m..0s]`
- `[up 9 steps]`
- `[volume inspect, 14 lines, exit 0]`
- `[1 line of 201 lines, 200 filtered, exit 1]`

## Error and refusal shape

- Invalid requests return readable tool errors explaining the fix.
- Refusals explain *why* a call is unsafe/unbounded and name a bounded alternative when possible.
- Per-target failures in batched operations are represented inline, not as whole-call protocol failure.

## Truncation behavior

Truncation is explicit, never silent:

- by line cap (`max_lines`)
- by byte budget (`max_bytes`)
- by per-tool bounded limits

Rendered output always indicates what was omitted.
