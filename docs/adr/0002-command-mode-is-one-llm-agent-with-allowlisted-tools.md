# Command mode is one LLM agent with allowlisted tools, and it absorbs the voice launcher

PARLA-55 proposed a voice launcher ("open Outlook") with its own LLM-free verb and alias matcher. On 2026-10-17 Mattia chose a single command hotkey instead. Each instruction goes to one LLM, which either replies with text for Parla to paste or calls a tool from a small, fixed allowlist that Parla defines, for example opening a URL in the default browser. This keeps v1 to one path and one hotkey, and the toolset grows one tool at a time. The cost is that "open X" waits for the LLM, about 1–1.5 s instead of PARLA-55's sub-second local match.

## Considered options

- One hotkey with a local fast path in front of the LLM (PARLA-55's matcher). Deferred: it gets built only if launching through the LLM feels slow in daily use.
- Command mode as text only, with a separate launcher hotkey. Rejected: it means two hotkeys and two pipelines.

## Consequences

Tools never include a shell or anything destructive. Each new tool is a deliberate addition to the allowlist, not something the user configures.
