# Parla

A Windows tray app that turns speech into text in whatever app has focus. It has two independent modes: dictation, and command mode.

## Language

**Dictation**:
The mode where Parla pastes the raw transcript of what you said into the focused app, unchanged.
_Avoid_: transcription mode, normal mode

**Command mode**:
The separate mode where what you say is an instruction, and Parla pastes an LLM's reply to it. It is the only mode that uses an LLM.
_Avoid_: AI mode, rewrite mode, command dictation

**Command hotkey**:
The chord that starts command mode, separate from the dictation hotkey. By default it is left Ctrl plus either Shift.
_Avoid_: command shortcut, second hotkey

**Instruction**:
What you say in command mode, transcribed. It is sent to the LLM and is never pasted itself.
_Avoid_: prompt, command text

**Selection**:
The text selected in the focused app, read once the command hold is confirmed. With no selection, the instruction stands alone and the reply is pasted at the cursor.
_Avoid_: highlighted text, input text

**Pill**:
The small on-screen overlay that shows recording, transcribing and the result. Command mode uses an orange pill, so it never looks like dictation.
_Avoid_: bar, overlay, HUD

**Tool**:
An action from Parla's fixed allowlist that command mode's LLM may ask Parla to perform instead of replying with text, such as opening a URL.
_Avoid_: action, skill, plugin, function

**Command run**:
One use of command mode, from the confirmed hold to its outcome: a pasted reply, a tool call, a failure or a cancel. Each run is recorded in history next to dictations, but is not counted as one.
_Avoid_: command dictation, LLM call, request
