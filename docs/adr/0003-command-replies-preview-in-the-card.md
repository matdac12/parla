# Command replies open in the Card; nothing is pasted without a click

Command mode v1 (PARLA-61) pasted the LLM's text reply straight over the selection, in one go. On 2026-10-17, while designing the Safety Net (PARLA-54), Mattia reversed that. Every command text reply now opens in the Card, the same floating panel that catches dictations with nowhere to go. He then picks Replace (or Paste), Copy or Dismiss. Often he wants to see a suggestion ("make this email more personal") before it touches his text. A question about a selection in a read-only view (a PDF, a chat transcript) has nowhere to paste at all. The LLM labels each reply as an **edit** or an **answer**, inside the strict JSON it already returns. That label only picks the Card's main button and the reply's tone; it never decides whether Parla pastes. Tools such as `open_url` still act at once, because an action has nothing to preview.

## Considered options

- Keep auto-paste, and let the LLM send only answers to the Card (a `show` flag). Rejected: a rewrite would still overwrite text Mattia hadn't seen.
- A separate `show_answer` tool. Rejected: it's a second way to return text, and per ADR-0002 a tool is an action.
- A local rule (a selection plus a question-shaped instruction means an answer). Rejected: too brittle in Italian and English.

## Consequences

Every command rewrite costs one click; Card v1 is mouse only. Don't turn auto-paste back on for "speed" without Mattia's decision. Dictation is unaffected: it still pastes at once, and reaches the Card only through the Safety Net.
