# Command tools may call Mattia's own services over the tailnet, and fail rather than queue

Until PARLA-82, every tool on command mode's allowlist (ADR-0002) acted on the local machine: `open_url` hands a page to the default browser. The `create_reminder` tool writes a To-do into Mattia's Traccia Personal Space. On 2026-10-17 Mattia chose plain REST for this (`POST /v1/todos`), reached only over his tailnet, with a dedicated token whose `todos` scope allows nothing beyond To-dos, the Project list and `/v1/me`. When Traccia can't be reached, the run fails loudly (the red pill and a notification) and nothing is queued. So a tool may now call a service Mattia runs himself, with a narrowly scoped secret stored in `settings.json` like the provider keys. It is still one LLM round trip: the tool's result never goes back to the model.

## Considered options

- Parla as an MCP client of Traccia's existing server. Rejected: Streamable HTTP and its auth are heavier in Rust than one REST call, and To-dos get no MCP tools in Traccia (its ADR 0017). A `todos` token gets 403 on `/mcp`.
- A local queue that retries when Tailscale comes back. Rejected: a reminder that is silently delivered late is worse than a visible failure, and the queue would be state that a lean tray app doesn't otherwise need. The instruction stays in history, so Mattia can say it again.
- A public endpoint. Rejected: Traccia's API stays tailnet-only.

## Consequences

- Only services Mattia owns qualify, each with a token scoped to what the tool needs. Never a general-purpose token (Traccia's `all` scope) and never a third-party account.
- Every network tool needs an explicit timeout, error mapping and a Settings "Test connection". Each one goes behind its own toggle, off by default.
- Nothing in this path runs during dictation (ADR-0001).
