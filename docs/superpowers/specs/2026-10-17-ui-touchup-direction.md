# UI touch-up direction (PARLA-57)

Picked by Mattia in the PARLA-57 thread on 2026-10-17. Design only: nothing here
is implemented yet. Prototype (throwaway, mock data, not bundled):
`prototypes/ui/parla-ui.prototype.html?variant=mix&accent=blue`. Open the file
directly or serve the folder (`npx vite prototypes/ui`).

## Decision

"Chosen mix" (variant 4 in the prototype): variant A's window, variant C's tray
flyout, today's pill, and Windows blue instead of the indigo accent.

## App window: one window, left sidebar

- Rail: logo + "Parla", a "Ready · <engine>" status line, then Home, History
  (with count), Timings, then a Settings group: Transcription, Dictation &
  hotkey, Other audio, Vocabulary, Clipboard. Benchmark ↗ link at the bottom
  (it keeps its own window).
- Home: shortcut reminder (push to talk, hands-free, cancel) with a Test mic
  button; a failure banner with Retry / Dismiss when the newest dictation
  failed; one stat strip (words, dictations, audio, cost with the yearly
  projection vs Wispr Flow); the 5 most recent dictations and "All history →".
- History: search, an All / Failed filter, rows grouped by day (time, text
  clamped to two lines, provider · audio · words, Copy / Retry on hover).
  Clear history stays at the bottom.
- Timings replaces the separate latency-log window (`history.html`): median,
  fastest and slowest tiles, one stacked bar per dictation (ready, transcribe,
  paste; colours from the dataviz reference slots 1-3, validated), a total per
  row, and the old table behind a "Table view" disclosure.
- Settings: today's settings, regrouped onto five short pages. A Save bar
  sticks to the bottom only while there are unsaved changes (Discard / Save).
  Provider and model switches still apply at once, as today.

## Tray

- Left-click: a small Windows-style flyout above the tray. Status + hotkey
  hint, provider segmented control (OpenAI / Grok / Local), three quick
  toggles (Live captions, Dim audio, Live mode), the last dictation with Copy,
  the next three as single lines with Copy / Retry. Footer: History, Timings,
  Test mic, ⚙ (opens the window on that page), Quit.
- Right-click: a native menu with Open Parla (default, bold), Settings…,
  Test mic (3 s), Retry last failed dictation, Quit Parla. History… and
  Benchmark… leave the menu.

## Overlay pill

Unchanged, by Mattia's explicit call, including its lilac hands-free tint. The
accent change does not reach it.

## Accent

Windows blue replaces indigo ("too much AI vibes"): `#0067c0` / hover `#005a9e`
on white text in light mode; `#4cc2ff` / hover `#7dd3ff` on `#002338` text in
dark mode. A fixed colour, not the user's Windows accent.

## Logo

Mattia likes the prototype's tray-icon placeholder: a small circle with a
dashed outline. It is only a placeholder: the tray icon belongs to PARLA-56,
so this is input for that issue, not part of this direction.

## Constraints for the implementation

- Nothing may touch the hotkey-to-paste path. The flyout is a separate webview
  created on demand from a tray click, never warmed at startup alongside the
  overlay.
- Measure the flyout's memory and open time before it ships.
- After any change near the hotkey hook or recorder, Mattia checks by hand
  (first syllable, AltGr, Esc when idle). The pill and recorder are out of
  scope, so this should not come up.
