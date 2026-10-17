# Command mode is the only LLM path; dictation stays raw

Parla's product rule says to paste the raw transcript and never add an LLM rewrite or cleanup pass. Command mode (PARLA-18) sends a spoken instruction, plus any selected text, to an LLM and pastes the reply. That is an LLM rewrite by design. On 2026-10-17 Mattia ruled that the rule is about **dictation**. Dictation keeps pasting the raw transcript and is never rewritten. Command mode is a separate, independent feature with its own hotkey, and it is the only place Parla calls an LLM. It must not change how dictation behaves or add cost to the dictation hotkey-to-paste path: dictation never waits on command mode, and the shared keyboard hook gains no per-keystroke key-state reads or OS calls (while command mode is off it costs one in-memory flag check per keystroke). Command mode is off until it is configured.

## Considered options

- Keep the rule absolute and move command mode into a separate app. Rejected because the recorder, ASR, pill and inserter would all be duplicated.
- Add an opt-in LLM cleanup to dictation. Rejected because dictation must stay raw and latency comes first.

## Consequences

AGENTS.md now scopes the "raw transcript" rule to dictation and names command mode as the only LLM path (approved by Mattia on 2026-10-17).
