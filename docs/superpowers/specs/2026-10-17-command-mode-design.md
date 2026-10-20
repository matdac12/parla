# Command mode v1 (PARLA-18)

> **Superseded in part (2026-10-17, PARLA-54):** text replies no longer paste automatically. They open in the Card (ADR-0003, `2026-10-17-safety-net-and-card-design.md`). The rest of this spec stands.

Design settled with Mattia on 2026-10-17 in a grill-with-docs session. Vocabulary: `docs/GLOSSARY.md`. Decisions with lasting weight: ADR-0001 (command mode is the only LLM path; dictation stays raw) and ADR-0002 (one LLM agent with allowlisted tools, which absorbs the PARLA-55 voice launcher).

## Problem Statement

Mattia dictates all day with Parla, and dictation is deliberately raw: what he says is pasted unchanged. He also often wants to *act* on text rather than add more of it: make a selected paragraph more formal, translate it to English, turn notes into bullets, or get a quick answer. He'd also like to open a web page by voice. Today that means copying text into a chat app, typing a prompt, waiting, and copying the result back, or reaching for the browser by hand. He wants these quick actions by voice, from any app, without changing or slowing down dictation in any way.

## Solution

Command mode is a separate, independent mode with its own hotkey. Mattia selects text in any app (or selects nothing), holds **left Ctrl + Shift**, says an instruction and releases. The pill turns **orange**, so it never looks like dictation. Parla transcribes the instruction with the same speech engine dictation uses and sends it, together with the selection, to an LLM (`gpt-6-luna` through the OpenAI Responses API). One of two things then happens:

- The LLM replies with text, and Parla pastes it in one go, over the selection if there was one, otherwise at the cursor. One Ctrl+Z brings back the original.
- The LLM calls a tool. In v1 the only tool is `open_url`: Parla opens the page in the default browser, and the pill shows "Opened <host>".

Command mode is off until Mattia enables it in Settings, and it needs an OpenAI key. With command mode off or idle, dictation behaves exactly as it does today.

## User Stories

1. As Mattia, I want command mode on its own hotkey, so that dictation keeps pasting my raw words and is never rewritten.
2. As Mattia, I want to hold left Ctrl + Shift to give a command, so that it feels like the dictation hold I already use.
3. As Mattia, I want either Shift key to work with left Ctrl, so that I can use whichever hand is free.
4. As Mattia, I want right Ctrl and AltGr to leave command mode alone, so that Italian typing never starts a command.
5. As Mattia, I want the command hotkey to be rebindable in Settings, so that I can move it if it collides with an app.
6. As Mattia, I want command mode to be hold only, so that it stays simple for quick interactions.
7. As Mattia, I want a press shorter than 300 ms to do nothing, so that quick Ctrl+Shift+T/N/V/Esc shortcuts keep working normally.
8. As Mattia, I want pressing any other key during the hold to cancel it silently, so that Ctrl+Shift shortcuts in my apps never trigger a command.
9. As Mattia, I want dimming of other audio to start only once the hold is confirmed, so that quick Ctrl+Shift shortcuts don't duck my music.
10. As Mattia, I want the pill to be orange in command mode, so that I always know whether I'm dictating or commanding.
11. As Mattia, I want my instruction transcribed by whatever engine dictation uses (Parakeet Ultra for me), so that there is nothing extra to configure and the transcription is fast and local.
12. As Mattia, I want Parla to read my selected text automatically, so that I don't have to copy it myself.
13. As Mattia, I want the selection read while I'm still speaking, so that it adds no wait after I release.
14. As Mattia, I want reading the selection never to interrupt a terminal (no Ctrl+C), so that a running Claude Code session is never killed.
15. As Mattia, I want "nothing selected" detected reliably, including VS Code's copy-the-whole-line behaviour, so that a command with no selection never overwrites a line I didn't select.
16. As Mattia, I want to select text and say "make this more formal", so that the formal version replaces my selection.
17. As Mattia, I want to say "translate to English" over a selection, so that the translation replaces it.
18. As Mattia, I want the reply to keep my selection's language, formatting and line breaks unless I ask otherwise, so that rewrites slot back in cleanly.
19. As Mattia, I want to give an instruction with nothing selected, so that the LLM's answer is pasted at my cursor.
20. As Mattia, I want only the result pasted, with no preamble, quotes or markdown fences, so that I never have to clean up the reply.
21. As Mattia, I want to say "open the X page", so that the page opens in my default browser.
22. As Mattia, I want to say "search Y", so that a search results page opens in my browser.
23. As Mattia, I want the pill to say which site it opened, so that I know the command worked.
24. As Mattia, I want only http and https URLs ever opened, so that a bad reply can't launch a local program or file.
25. As Mattia, I want the reply pasted in one go, so that one Ctrl+Z restores my original text.
26. As Mattia, I want my clipboard restored after a command, the same as after dictation, so that what I had copied is not lost.
27. As Mattia, I want Esc to cancel a command while I'm recording or while Parla waits for the LLM, so that I can back out without anything being pasted.
28. As Mattia, I want an empty or silent instruction to do nothing at all, so that a stray hold costs nothing and leaves no trace.
29. As Mattia, I want a failure (network, timeout, API error, empty reply) to show the red cross and the usual notification, so that I know nothing was pasted.
30. As Mattia, I want a request that takes longer than 20 s to fail instead of hanging, so that the pill never sits there forever.
31. As Mattia, I want every command run recorded in history, with its instruction, outcome, model, tokens and timings, so that I can see what happened and how long it took.
32. As Mattia, I want command runs tagged orange in the main window's list, with Copy copying the reply, so that I can reuse a reply.
33. As Mattia, I want history to record only the length of my selection, not its text, so that private text I rewrote is not kept on disk.
34. As Mattia, I want command runs kept out of my dictation totals (words, dictations, audio), so that those stats stay about dictation.
35. As Mattia, I want each command's LLM cost added to Total cost, so that my cost estimate stays honest.
36. As Mattia, I want my existing OpenAI key reused, so that there is no new secret to store.
37. As Mattia, I want `OPENAI_API_KEY` to override the stored key, the same as for dictation, so that the key behaves consistently.
38. As Mattia, I want my requests sent with storage turned off, so that my selections don't sit on OpenAI's servers.
39. As Mattia, I want command mode off by default behind an Enabled toggle in a "Command mode" Settings card, so that nothing changes until I opt in.
40. As Mattia, I want the toggle disabled with a hint when no OpenAI key is set, so that I know what's missing.
41. As Mattia, I want the hook to ignore Ctrl+Shift completely while command mode is off, so that it costs nothing when unused.
42. As Mattia, I want the model and the reasoning effort as settings, so that we can tune them without a rebuild.
43. As Mattia, I want the default reasoning effort chosen by a measured eval, so that we pick `none` if it's good enough and `low` only if it isn't.
44. As Mattia, I want presses ignored while a command run is in flight, so that a dictation and a command never race for the clipboard.
45. As Mattia, I want dictation completely unaffected when command mode is off or idle, so that my current workflow keeps feeling instant.
46. As Mattia, I want new tools added one at a time to a fixed allowlist, so that command mode grows deliberately and can never do anything destructive.
47. As a future agent working on Parla, I want the product rule to say clearly that command mode is the only LLM path, so that I neither block command mode nor add an LLM to dictation.

## Implementation Decisions

**Product boundary**
- Command mode is the only code path that calls an LLM (ADR-0001). Dictation code is untouched apart from the shared keyboard hook and the shared session guard. The hook gains no per-keystroke key-state reads: the 15/10 fix took those out because they cost about 250 µs a key.

**Hotkey and session**
- The hook learns a second chord: left Ctrl plus left or right Shift. Right Ctrl doesn't count, and left Alt or AltGr breaks the chord.
- It reuses the existing hold events (chord pressed, released, cancelled by another key, Esc). The events carry which chord fired, either dictation or command.
- The chord is armed only while command mode is enabled.
- Command mode is hold only: no latch, no hands-free mode, no Space conversion. It shares dictation's 300 ms minimum hold and 120 s cap.
- One session at a time: the existing single-session guard covers command runs from the confirmed hold until the paste, the tool action, a failure or a cancel. Hotkey presses during a run are ignored.
- Settings gains a `command` section: `enabled` (default false), `hotkey` (default LCtrl+Shift), `model` (default `gpt-6-luna`) and `reasoning_effort` (default chosen by the eval).

**The command run**
- **Key-down:** recording starts at key-down with the normal pre-roll, so the first syllable is kept. The pill shows (orange) after the usual delay. Dimming waits until the 300 ms hold is confirmed.
- **Hold confirmed (300 ms):** Parla reads the selection while you are still speaking:
  1. Save the user's clipboard text.
  2. Note the clipboard sequence number.
  3. Send **Ctrl+Insert**, never Ctrl+C. It reuses the inserter's existing modifier handling: it borrows a held Ctrl and releases a stray Shift, Alt or Win.
  4. Poll the clipboard sequence number for up to about 150 ms.
  5. A changed sequence number with non-empty text counts as the selection. VS Code's empty-selection line copy (marked as such in its clipboard data) counts as no selection.
  6. It also records the foreground app's executable name.
- **Key-up:** the instruction is transcribed by the active dictation provider.
  - An empty or whitespace-only instruction ends the run silently: no LLM call, no history row, and the clipboard is restored.
- **LLM request:** a single, non-streaming request to the OpenAI Responses API (never Chat Completions). It sends:
  - The built-in system prompt.
  - The instruction, with the selection in a clearly delimited block. The selection is treated as data, never as instructions.
  - The foreground app name and today's date.
  - `store: false`.
  - The `open_url` tool, declared with a strict schema. `tool_choice: auto`, no parallel tool calls.
  - The configured reasoning effort.
  - A cap of about 2,000 output tokens.
  - A 20 s timeout.
- **Reply handling:** the Responses output items decide the outcome.
  - An `output_text` item is pasted once, over the selection or at the cursor. It uses the inserter's paste, then restores the clipboard once.
  - A `function_call` item for `open_url` is run by Parla: the URL must parse as http or https, and it opens through the shell's default handler. Nothing goes back to the model and nothing is pasted. The pill shows "Opened <host>", then the check.
  - Anything else counts as a failure: an error, a timeout, an empty text, a URL that isn't http(s), an unknown tool.
  - There's no focus guard: the reply is pasted into whatever has focus, by Mattia's choice.
- **Cancel:** Esc during the recording or the wait ends the run. A late reply is discarded and nothing is pasted or opened.
- **Failure:** the red-cross pill and the existing error notification, with no Retry in v1.

**System prompt (starting point; refine it while testing)**
- "You are Parla's command mode. Apply the instruction to the selected text and output only the resulting text: no preamble, quotes or markdown fences."
- "Keep the selection's language, formatting and line breaks unless the instruction says otherwise."
- "With no selection, answer the instruction briefly as plain text ready to paste."
- "If the user asks to open or search a page, call `open_url` and output nothing else."

**Modules**
- **Command run orchestrator (new, deep):** the whole run behind one function. It talks to narrow ports: desktop (clipboard read/write, copy keystroke, clipboard sequence, paste, foreground app name), LLM, URL opener and history sink. It owns the selection detection, the empty-instruction rule, outcome handling, the clipboard restore and cancellation.
- **Responses client (new):** pure request building and response parsing, plus a thin HTTP call on the existing `reqwest` blocking client. It adds no new crate.
- **Tools (new):** the allowlist, which holds only `open_url` in v1. Each tool is a JSON schema plus a validator and an executor.
- **Hotkey hook (modified):** the second chord, armed by setting.
- **Flow controller (modified):** the command phase (hold only), sharing the session guard.
- **Overlay (modified):** the orange mode for the pill, and the "Opened <host>" result.
- **History, stats and pricing (modified):**
  - A history `kind: "command"` row holds the instruction, whether there was a selection and its length, the outcome (pasted, tool and URL, failed, or cancelled), the model, the input and output tokens from the response's `usage`, and stage timings (key-up, transcribed, LLM reply, pasted or opened).
  - Totals exclude command runs from words, dictations and audio. Total cost adds the LLM token cost at `gpt-6-luna`'s list price; the instruction's ASR cost counts as it does today.
- **Settings UI (modified):** a "Command mode" card with Enabled, hotkey, model and effort. The card is disabled while no OpenAI key is set.
- **Main window list (modified):** command rows with an orange tag. Copy copies the reply.

## Testing Decisions

- Good tests check external behaviour through a module's interface (what got pasted, opened, written to history, restored to the clipboard), never internal calls or state.
- **Command run orchestrator:** the main seam, tested with fake ports. Cases:
  - A selection gets replaced, and the clipboard is restored.
  - With no selection, the reply goes in at the cursor.
  - VS Code's empty-selection copy counts as no selection.
  - `open_url` opens the URL and pastes nothing.
  - A non-http(s) URL and an unknown tool both fail.
  - An empty instruction makes no LLM call and writes no row.
  - A timeout or API error fails with nothing pasted.
  - Esc during the wait means a late reply is discarded.
  - The history row's fields and the cost are written.

  Prior art: the inserter's `Desktop` fake and the flow's `RecoverySource` fake.
- **Hotkey hook:** the existing hook tests are extended:
  - Left Ctrl plus either Shift fires the command chord. Right Ctrl doesn't.
  - Another key cancels the chord.
  - The chord stays silent while disabled.
  - Dictation's Ctrl+Alt behaviour is unchanged.
- **Flow controller:** the existing `on_event` tests extended for the command phase: a press under 300 ms cancels, there's no latch, and presses during a run are ignored.
- **Responses client:** request-building and response-parsing tests against recorded fixture JSON, with no network. They cover `store: false`, the strict tool schema, the effort, the delimited selection, and parsing text replies, function calls, `usage` and errors.
- **Live eval:** about 20 fixed IT/EN instructions (rewrite, translate, answer with no selection, open a page, search). Each runs 3× at effort `none` and `low`, recording whether it chose the tool correctly and the time to reply. It's gated on `OPENAI_API_KEY`. It picks the default effort and is re-run whenever a tool is added. Prior art: the OpenAI live test.
- **Settings:** an old settings file loads `command.enabled = false` and the defaults. Prior art: the core contracts test for old settings.
- **Hand checks (required by AGENTS.md after hook changes):**
  - Dictation still keeps the first syllable.
  - AltGr typing works.
  - Esc passes through when idle.
  - Quick Ctrl+Shift shortcuts still work.
  - The Ctrl+Insert app matrix: Notepad, Word, Outlook, Edge/Brave, the VS Code editor and terminal, Windows Terminal, WhatsApp, Claude.

## Out of Scope

- Any LLM step in dictation.
- Hands-free or latched command recording.
- Streaming the reply, or a streaming setting.
- Every tool except `open_url`. `open_app` (focus or launch an app) is the next planned iteration.
- The local LLM-free fast path for "open X" from PARLA-55. It gets built only if launching through the LLM feels slow.
- An approval card before replacing text, and a focus-change guard.
- PARLA-54 (the Safety Net card), which stays a separate feature.
- Retry for failed command runs.
- A user-editable system prompt.
- LLM providers other than OpenAI.
- A multi-step agent loop that sends tool results back to the model.
- Using the Caps Lock hyper key, or any injected-key trigger, for the command hotkey.

## Further Notes

- Research (2026-10-17, official docs; latency from Artificial Analysis): `gpt-6-luna` costs $0.10/$0.50 per million tokens in/out, with about 0.94 s to first token at effort `none`. A command run of about 1k tokens in and 300 out costs well under $0.001. The effort parameter defaults to `medium`, so it must be set explicitly.
- The Responses API stores requests by default, so `store: false` is required.
- PARLA-13's missing clipboard receipt is partly addressed here: reading the selection checks the clipboard sequence number.
