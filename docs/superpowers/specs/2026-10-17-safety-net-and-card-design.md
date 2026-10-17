# Safety Net and the Card (PARLA-54)

Design settled with Mattia on 2026-10-17 in a grill-with-docs session. Vocabulary: `GLOSSARY.md` (Card, Safety Net, Edit and Answer). ADRs:
- ADR-0001: dictation stays raw; command mode is the only LLM path.
- ADR-0002: one LLM agent with allowlisted tools.
- ADR-0003 (new): command replies open in the Card; nothing is pasted without a click.

Look: `prototypes/ui/card.prototype.html`, **variant A** (Mattia's pick).

## Problem Statement

Parla pastes blind. If Mattia dictates while the focus is on the desktop, a game, a read-only view or a window that just closed, Ctrl+V lands nowhere. The text survives only in history, and on the clipboard for the 250 ms before the restore.

Command mode v1 has the opposite problem: it always pastes. If Mattia selects a clause in a PDF, or a message in a chat transcript, and asks a *question* about it, the answer has nowhere sensible to go. And when he asks for a rewrite ("make this email more personal"), he often wants to see the suggestion before it replaces his text.

## Solution

One new surface, the **Card**. It's a small dark panel above the pill, styled like the streaming captions box. It shows text Parla didn't paste and offers **Replace** or **Paste**, **Copy** and **Dismiss**. It never takes focus, so the app underneath keeps its focus and its selection.

The Card is fed two ways:

- **Safety Net (dictation).**
  - While Mattia speaks, a UI Automation probe checks whether the focused element accepts text.
  - On a confident "not editable", the transcript goes to the Card instead of being pasted.
  - When the probe is unsure, Parla pastes as today.
  - It adds nothing to the key-up→paste path.
- **Command replies (always).** Every command text reply opens in the Card (ADR-0003).
  - The LLM labels the reply as an **edit** (meant for the document: Replace or Paste is the main button) or an **answer** (meant for Mattia: Copy is the main button).
  - `open_url` still opens at once.

## User Stories

1. As Mattia, I want a dictation that has nowhere to land to show up in the Card, so that I never lose what I said.
2. As Mattia, I want the editable check to run while I'm speaking, so that dictating into a text field is exactly as fast as today.
3. As Mattia, I want Parla to paste whenever it isn't sure, so that terminals, Electron apps and custom editors keep working.
4. As Mattia, I want the Safety Net on by default with a Settings toggle, so that I can turn it off if it ever misfires.
5. As Mattia, I want every Safety Net decision logged, so that false negatives can be tuned.
6. As Mattia, I want every command reply shown in the Card first, so that nothing replaces my text until I choose to.
7. As Mattia, I want to select text in a PDF or a chat and ask a question about it, so that the answer is shown to me instead of being pasted nowhere.
8. As Mattia, I want the Card's main button to fit the reply (Replace for an edit, Copy for an answer), so that the likely action is one obvious click.
9. As Mattia, I want answers written to me conversationally and in my language, and edits written as ready-to-paste text, so that each reads right.
10. As Mattia, I want Replace to paste over the selection I still have in the app, so that it works like v1's paste, one click later.
11. As Mattia, I want Replace or Paste hidden when nothing editable has focus, so that the Card only offers what can work.
12. As Mattia, I want Copy to put the text on my clipboard for good, so that I can paste it wherever I want.
13. As Mattia, I want Paste to go into whatever field I clicked into last, so that a false negative costs me one click.
14. As Mattia, I want the Card to stay until I act on it or start the next dictation or command, so that I can read a long answer without it vanishing.
15. As Mattia, I want Esc to keep passing through untouched, so that the Card never changes how my keyboard behaves.
16. As Mattia, I want the Card to look like the captions preview (minimal, dark, above the pill), so that it feels part of the same tool.
17. As Mattia, I want the Card at the bottom centre where the pill is, so that my eye doesn't travel to a hidden taskbar corner.
18. As Mattia, I want my instruction shown in the Card's header, so that I can see what Parla heard.
19. As Mattia, I want long replies to scroll inside the Card, so that it never covers the screen.
20. As Mattia, I want `open_url` to keep opening the page at once, so that launching stays one step.
21. As Mattia, I want a caught dictation still counted in my dictation stats, so that my numbers stay honest.

## Implementation Decisions

**Product boundary**
- Dictation still pastes the raw transcript at once whenever the focus is (or might be) editable. The Safety Net never adds a wait: the probe's result is read without blocking, and a probe that hasn't answered counts as "editable".
- Command mode never pastes without a click (ADR-0003). Tools act at once.
- All UIA and window code goes behind `cfg(windows)`. On other targets the probe always answers "editable".

**The editable probe (Rust, new module)**
- It needs the `Win32_UI_Accessibility` feature of the `windows` crate. It runs on one long-lived probe thread, created lazily on the first session, never at startup. COM (MTA) and a cached `IUIAutomation` are made only on the first probe that needs UIA (the shell or no window), so a day without one never creates a UIA client.
- **When it runs:** it starts at **key-down** for dictation (hold, latch or hands-free) and for command (it is discarded if the command hold is cancelled). It runs in parallel with recording.
- **What it reads:** the foreground window's class first; UIA only where its answer can change the verdict.
  - **An app has the foreground:** no UIA call at all. **Unknown** (rule `App`), so it pastes as today.
  - **The shell has the foreground** (Progman, WorkerW, Shell_TrayWnd, Shell_SecondaryTrayWnd): UIA's focused element.
    - **Editable** if any of these holds (a desktop rename box, the taskbar's search box):
      - the control type is Edit or Document;
      - a ValuePattern is present and `IsReadOnly` is false;
      - a TextPattern is present and the element is keyboard-focusable.
    - Otherwise, a UIA error included, **Not editable** (rule `Shell`): the window class is the confident signal.
  - **No foreground window:** UIA's focused element and the top-level window (or, without one, the process) that holds it.
    - An editable element: **Editable**, as above.
    - Focus on the shell (a shell window class, the desktop root, or explorer.exe with no window) or no focused element: **Not editable** (rule `NoForeground`).
    - Focus in another app (mid Alt+Tab, a window just closed): **Unknown** (rule `App`).
    - A UIA error: **Unknown** for now (never stop a paste that would have worked); revisit once a real desktop click is logged.
  - **Unknown** also covers no answer by the time it's needed. It counts as editable.
  - 17/10, PARLA-79 hand check: WhatsApp's message box read as a bare Pane and a PDF viewer's page as a Window or a Menu; neither matched a rule, and Mattia's rule is to never stop a paste that would have worked. The desktop read as no foreground window.
  - 17/10, Mattia: UIA only on the shell/no-window path; no app is queried. Inside an app every UIA answer pasted anyway, so the call was pure cost, and it may switch on Chromium's accessibility tree (WhatsApp's WebView2 grew after the hand check).
- **When it's read:** at the moment the paste would happen, after transcription, so the probe has the whole hold plus transcription to answer. The read never waits.
- **Focus changes during the hold** are not handled (Mattia: he doesn't switch context mid-command). No second probe at key-up.
- **Log** each decision: verdict, app executable, foreground window class, which rule fired, probe time and, when UIA was asked, the control type and where the focused element lives.

**Safety Net (dictation)**
- Setting: `safety_net: bool`, default **true**, a flat field like `auto_send`. It sits in Settings next to the paste options. It is not added to the flyout's quick toggles.
- **Not editable:** no paste, and the clipboard is left untouched. The transcript opens in the Card with a neutral tag, "Not pasted · no text field". Copy is the main button, Paste is secondary.
- Auto-send (PARLA-58) never fires from the Card.
- **Editable or unknown:** today's path, unchanged.

**Command mode changes (on top of PARLA-61)**
- The strict JSON schema becomes `{"text": string, "kind": "edit" | "answer"}`, both required.
- **Prompt additions:**
  - "edit" is for text meant to go into the document: a rewrite, a translation, a drafted text, an inserted value.
  - "answer" is for text addressed to the user: explanations, opinions, facts about the selection. Write answers conversationally, in the user's language. They are still plain text, no markdown.
- **Outcome:**
  - A text reply opens in the Card. It is never auto-pasted.
  - The selection-read clipboard is given back right away (`give_back`), since nothing is pasted yet.
  - `open_url` is unchanged: it opens at once, and the pill shows "Opened <host>".
- **Buttons**, from `kind`, the selection and the probe:
  - Editable with a selection: **Replace** (main for an edit), Copy, Dismiss.
  - Editable with no selection: **Paste** (main for an edit), Copy, Dismiss.
  - Not editable: **Copy**, Dismiss.
  - For an answer, Copy is the main button and Replace or Paste is secondary.
- **Re-run the live eval** with a `kind` check on each case: rewrite and translate are edits, questions about a selection are answers. Re-check the default effort if `kind` accuracy at `low` is poor.

**The Card**
- **Window:** reuse the overlay window; don't add a new webview (lean memory, as with the captions). It grows to fit the Card above the pill and is click-through except over the Card, the same as the failed pill's Dismiss and Retry (`set_interactive`). It stays non-focusable.
- **Placement:** the pill's anchor: the primary monitor, horizontally centred, above the pill. The pill below shows the check (orange outline for command).
- **Look:** prototype variant A.
  - Width 420 logical px, with the captions colours (`rgba(18,18,20,.88)`, a hairline ring, 11px radius, Segoe UI 13/19).
  - Header: a tag (an orange dot and "Command", or a neutral dot and "Not pasted · no text field"), the instruction quoted in muted text, and ✕.
  - Body: plain, selectable text that scrolls past about 12 lines (228 px).
  - Footer: right-aligned pill buttons, with the main one filled white.
- **Actions:**
  - **Replace/Paste:** the normal paste transaction into the current focus, including the clipboard restore, with no focus guard. Then the Card closes.
  - **Copy:** set the clipboard with no restore, then close.
  - **Dismiss:** close.
- **Lifetime:** the Card stays until a button closes it or the next dictation or command starts. No timer, no Esc, no keyboard control in v1. The hook is unchanged.
- **Wait state:** the orange spinner pill as today, until the reply is complete. No streaming.
- 17/10, PARLA-80 as built:
  - The window takes the Card's tallest size (456 × 380) while it is open. A Windows click-through flag covers a whole window, so a thread polls the cursor every 30 ms while the Card is open and makes the window clickable only over the Card's box (reported by the page). The rest, the check included, stays click-through.
  - A caught dictation shows what would have been pasted: with auto-send on, without its trigger word. History keeps the raw transcript.

**History and stats**
- A caught dictation is a normal dictation row with outcome `shown`. It counts in words and totals as today.
- A command run whose reply went to the Card gets outcome `shown`. Its `reply` and `kind` are stored, plus the LLM fields as today.
- The Card action (replaced, pasted, copied, dismissed) is written to the log. History stays append-only (see Open Questions).

## Testing Decisions

- **Probe:** a pure decision function (window and UIA facts in, verdict out) is unit-tested on all platforms: an app in the foreground (Unknown, never queried); on the shell, each editable rule winning and anything else, an error included, not editable; with no foreground window, focus on the shell or nowhere (not editable), focus in an app (Unknown), an editable element (editable) and a UIA error (Unknown). The COM wrapper is thin and checked by hand.
- **Flow:**
  - A fake probe source covers a dictation that is not editable (Card, no paste, no clipboard write), an unknown verdict (paste), and a probe that hasn't answered when the paste is due (paste, no wait).
  - Prior art: `RecoverySource` and the `on_event` tests.
- **Command orchestrator:**
  - An edit reply with a selection shows the Card with Replace as the main button and the clipboard given back.
  - An answer reply shows Copy as the main button.
  - A reply when not editable shows Copy only.
  - `open_url` is unchanged.
  - A `kind` missing from the JSON fails the parse.
  - Prior art: the fake ports in `command_mode`.
- **Responses client:** schema and parse fixtures for `kind`.
- **Live eval:** `kind` accuracy added to the existing about-20-case eval.
- **Hand checks (AGENTS.md, since this touches the paste flow):**
  - first syllable kept;
  - AltGr typing;
  - Esc passes through when idle;
  - dictating onto the desktop opens the Card;
  - dictating into Notepad, Windows Terminal, VS Code, T3 Code, WhatsApp and Outlook pastes;
  - a command over a PDF selection gives an answer with Copy as the main button (an app in the foreground is Unknown since 17/10, so Replace stays as the secondary button);
  - the Card's buttons don't steal focus, and Replace lands over the live selection.

## Out of Scope

- Follow-up questions in the Card (a conversation). Mattia: probably never.
- Streaming the reply into the Card.
- Keyboard control of the Card (Esc to dismiss, a hotkey for the main button), to be revisited after daily use.
- "Paste where I click next" (it would need a global mouse hook). It is replaced by Paste into the current focus.
- A focus guard on Replace.
- A second probe at key-up for focus changes during the hold.
- A per-app allow or deny list for the probe. Add one only if the logs show recurring false negatives.

## Settled after the session (Mattia accepted the recommendations)

- **The Card action in history:** history stays append-only. The action (replaced, pasted, copied, dismissed) is written to the log only.
- **Multi-monitor:** the Card follows the pill to the primary monitor.
- **The Safety Net toggle** is in Settings only, not in the flyout.
- **The Card never fires auto-send.**

## Tickets

Sub-issues of PARLA-54, each blocked by the one before:
- PARLA-79: the editable probe, logged, behind a setting.
- PARLA-80: the Card, and the Safety Net for dictation.
- PARLA-81: command replies open in the Card.
