# Parla

A Windows tray app that turns speech into text in whatever app has focus. It has two independent modes: dictation, and command mode.

## Language

**Dictation**:
The mode where Parla pastes the raw transcript of what you said into the focused app, unchanged.
_Avoid_: transcription mode, normal mode

**Command mode**:
The separate mode where what you say is an instruction, and Parla shows an LLM's reply to it in the Card, or runs a tool. It is the only mode that uses an LLM.
_Avoid_: AI mode, rewrite mode, command dictation

**Command hotkey**:
The chord that starts command mode, separate from the dictation hotkey. By default it is left Ctrl plus either Shift.
_Avoid_: command shortcut, second hotkey

**Instruction**:
What you say in command mode, transcribed. It is sent to the LLM and is never pasted itself.
_Avoid_: prompt, command text

**Selection**:
The text selected in the focused app, read once the command hold is confirmed. With no selection, the instruction stands alone and Replace becomes Paste at the cursor.
_Avoid_: highlighted text, input text

**Pill**:
The small on-screen overlay that shows recording, transcribing and the result. Command mode uses an orange pill, so it never looks like dictation.
_Avoid_: bar, overlay, HUD

**Tool**:
An action from Parla's fixed allowlist that command mode's LLM may ask Parla to perform instead of replying with text, such as opening a URL.
_Avoid_: action, skill, plugin, function

**Command run**:
One use of command mode, from the confirmed hold to its outcome: a reply shown in the Card, a tool call, a failure or a cancel. Each run is recorded in history next to dictations, but is not counted as one.
_Avoid_: command dictation, LLM call, request

**Card**:
The floating panel above the pill that shows text Parla did not paste (a command mode reply, or a dictation caught by the Safety Net), or briefly confirms a Reminder the command created. It never takes focus.
_Avoid_: popup, toast, preview window, answer window, safety net window

**Safety Net**:
The rule that sends a dictation to the Card instead of pasting it when nothing editable has focus. It only acts on a confident "not editable"; when unsure, Parla pastes. Its setting (`safety_net`) also gates the editable probe that command replies use to pick the Card's buttons.
_Avoid_: fallback, editable check, focus guard

**To-do**:
An item in Mattia's Traccia Personal Space that he means to do, optionally on a Planned day. Command mode's reminder tool creates To-dos; it never creates Traccia Issues.
_Avoid_: task, issue, promemoria

**Reminder**:
A time of day on a To-do's Planned day when Traccia pings Mattia on Telegram. A To-do has at most one. Parla sets it but never delivers it.
_Avoid_: alarm, notification, due date

**Project reference**:
An `@KEY` in a To-do's title that names one of Mattia's Traccia Projects (for example `@CRM`). The LLM writes it from the Project list Parla puts in the request.
_Avoid_: tag, project link, mention

**Edit** and **Answer**:
The two kinds of command reply. An edit is text meant to go into the document (a rewrite, a translation, a drafted email), so the Card's main button is Replace or Paste. An answer is addressed to the user (an explanation, a calculation), so the main button is Copy. The LLM says which kind it wrote.
_Avoid_: rewrite vs question, action vs info
