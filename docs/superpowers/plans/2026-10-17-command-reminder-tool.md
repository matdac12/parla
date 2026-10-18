# Command reminder tool: implementation plan (PARLA-82)

> **For agentic workers:** one slice per ticket, each blocked by the one before. Work test-first (mattpocock-skills:tdd), in a worktree on `parla-NN-slug`. Every commit names the ticket. Mattia answered the spec's open questions on 17/10: see its **Decisions** section.

**Goal:** saying "ricordami di chiamare Luca domani alle 15" in command mode creates a To-do with a Reminder in Mattia's Traccia Personal Space, and the Card confirms it with Open and Undo.

**Architecture:**
- One more allowlisted tool, `create_reminder`, answered in the same single LLM round trip.
- The model gives the title, the local day and the local time.
- Pure Rust (`when.rs`) turns those into a UTC `remindAt`, a day only, or Anytime.
- A small Traccia REST client (`traccia.rs`) makes the POST on the existing blocking `reqwest` client.
- The Project list is cached in memory and refreshed at the confirmed command hold.

**Tech stack:**
- Rust: `reqwest` blocking (already a dependency) and `windows-sys` `Win32_System_Time` (a feature, not a crate);
- TypeScript for the Settings block and the Card.

**Spec:** `../specs/2026-10-17-command-reminder-tool-design.md`. ADRs: 0001, 0002, 0003 and 0004.

## Global constraints

- **Nothing on the dictation path:**
  - no change to `hotkey.rs`, the recorder, the inserter, or `flow.rs`'s dictation branch;
  - the Projects refresh runs only at a confirmed command hold, and only with `command.reminders` on.
- **Tool off means byte-identical:** the same request body, prompt and tools as today. A test pins it.
- **Secrets:**
  - never write the token or the real base URL to the repo, Traccia, logs or fixtures (use `https://traccia.example.ts.net` and `test-token`);
  - mark the bearer header sensitive;
  - keep only the error `code` and `details.fields` names from Traccia's error bodies.
- **Explicit timeouts:** 5 s connect (shared client), 5 s for the POST and `DELETE`, 2 s for the Projects GET.
- **Windows-only code behind `cfg(windows)`:** CI lints and tests on Linux too.
- **The usual checks:** from `src-tauri/`, `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked`; from the root, `npm ci && npm run build`.

## Slice 1: the Traccia connection (settings, client, Test connection)

**Files:**
- new `src-tauri/src/command_mode/traccia.rs`;
- `settings.rs`;
- `commands/settings.rs` (or a new `commands/traccia.rs`);
- `src/settings.ts` and `src/records.ts`.

**Build:**
- **Settings:**
  - `CommandSettings.reminders: bool`, default false;
  - a `providers.traccia {base_url, token}` section, read by a `TracciaConfig::resolved(settings)` with the `TRACCIA_URL` and `TRACCIA_TOKEN` env overrides;
  - the base URL trimmed, with no trailing slash, http or https only.
- **`traccia.rs`:**
  - `Client {base, token}`, plus `me()` and `projects()`;
  - pure `parse_*` functions for `/v1/me` (`{actor, tokenName}`), the projects page (`{items:[{key,name,status}], nextCursor}`) and the error body;
  - an error type that maps the transport, 400, 401, 403 and 5xx failures to the spec §5 messages.
- **Tauri command `test_traccia`:** it returns "Connected as you · token parla · N projects", or the §5 message, and warns when the actor isn't `you`.
- **Settings UI:**
  - a "Reminders in Traccia" toggle in the Command mode card, disabled with a hint until command mode is on and a URL and token are set;
  - a URL field with the placeholder `https://<traccia-host>.<tailnet>.ts.net`, and a password field for the token;
  - a Test connection button that shows the result inline;
  - `save_settings` refuses `reminders: true` without a URL and a token.

**Tests:**
- an old settings file loads `reminders: false` and keeps `providers.traccia` untouched;
- URL normalisation;
- parse fixtures for `me`, `projects` (active and paused kept, completed and canceled dropped) and each error class;
- error text never contains the server's message or the token.

**Done when:** Mattia pastes a URL and a `todos` token, presses Test connection, and sees "Connected as you · token parla · N projects". With Tailscale off he sees "Traccia can't be reached. Is Tailscale on?" within about 5 s.

## Slice 2: `create_reminder` creates a To-do (pill only), plus the eval

**Files:**
- `command_mode/tools.rs`, `command_mode/responses.rs` and `command_mode/mod.rs`;
- new `command_mode/when.rs`;
- `traccia.rs` (`create`);
- `timing.rs` (`CommandOutcome::Created`);
- `flow.rs` (the real ports only);
- `src/overlay.ts` (pill label);
- `tests/command_eval.rs`;
- `Cargo.toml` (`Win32_System_Time`).

**Build:**
- **The tool:** `Tool::CreateReminder {title, day, time}`.
  - The strict declaration: `day` `["string","null"]` with format `date`, and `time` `["string","null"]` with pattern `^([01][0-9]|2[0-3]):[0-5][0-9]$`.
  - The validator: a trimmed title of 1–500 characters, a real date, a valid time.
  - `declarations(reminders_on)`.
- **The request:**
  - `REMINDER_RULES` (the spec §1 prompt rules, starting from the probe's final wording) is appended to `SYSTEM_PROMPT` only while the tool is on;
  - `Run.date` becomes `Run.now`, and the request says `Now: Saturday 2026-10-17 21:14`.
- **`when.rs`:**
  - `resolve(day, time, now) -> When`, through a `LocalClock` port;
  - `DEFAULT_REMINDER_TIME = 09:00` for a day with no time (Q2);
  - local to UTC via `TzSpecificLocalTimeToSystemTime` on Windows; off Windows, `day` and `time` are passed through.
- **`traccia.rs`:** `create(title, When) -> Created {id, url, title, day, time}`, with a 5 s timeout.
- **The orchestrator:**
  - `Ports.reminders: Option<&dyn Reminders>`;
  - `run_tool` handles `CreateReminder`;
  - Esc is honoured until the POST starts, then ignored;
  - `Outcome::Reminded {title, day, time, url, id}`;
  - the history row: `created`, `tool`, `url`, `reply = "<title> · <day> <time>"`.
- **The pill:** "Reminder set" with the check (the Card arrives in slice 4).
- **The eval:**
  - add the probe's reminder cases and controls (spec §Testing), graded on the day, the time and the title, against a fixed Now passed through `Request`;
  - re-run all 3× at `low` and `none`;
  - record the accuracy and the median and p90 reply time, taking the timed run under `%LOCALAPPDATA%\parla-machine.lock` when the orchestrator assigns it.

**Tests:** `when.rs`, the table and the DST edges with a fake clock; the tool declaration pinned; the tool-off request byte-identical; the orchestrator cases from the spec with fake `Reminders`; the create-request and response fixtures.

**Done when:**
- "ricordami di chiamare Luca domani alle 15" creates the To-do (checked in `/personal`), and the pill says "Reminder set";
- a past time gives the red pill with "…has already passed";
- the eval's reminder cases pass at `low`, with no regression on the 31 existing ones.

## Slice 3: Project references

**Files:** `traccia.rs` (the cache), `flow.rs` (one call at the confirmed command hold), `responses.rs` and `mod.rs`.

**Build:**
- **The cache:** `ProjectsCache` (`Mutex<Option<(Instant, Vec<(key, name)>)>>` plus an in-flight flag).
  - `refresh_if_stale()` runs at the confirmed hold, on a background thread, with a 10 min TTL and a 2 s timeout, and keeps the last list on failure.
  - `snapshot(wait)` returns the cached list, waiting at most 500 ms only when the cache is empty and a fetch is in flight.
- **The request:** `user_text` writes `Projects:` with one `KEY: Name` line each, only when there is a list.
- **The prompt:** the rule to append `@KEY` when a listed Project is named (it may already be in slice 2's wording; keep it there).
- **The eval:** cases with `@CIT` and `@CRM`, and a Project that isn't listed, which must get no `@KEY`.

**Tests:**
- cache staleness and in-flight behaviour with a fake clock and fetcher;
- the request with and without `Projects:`;
- the refresh never runs while the tool is off.

**Done when:** "…per Cittadellese" gives a To-do with the Cittadellese chip, and the first command after starting Parla already has the list.

## Slice 4: the reminder Card (Open and Undo)

**Files:**
- `card.rs`, `commands/card.rs` and `overlay.rs` (the Card's payload);
- `src/overlay.ts` and `src/overlay.css`;
- `command_mode/mod.rs`;
- `traccia.rs` (`undo`).

**Build:**
- **`card.rs`:**
  - `Tag::Reminder` (an orange dot, "Reminder");
  - `Action::Open` and `Action::Undo`;
  - a `Card::reminder(instruction, title, when_line, id, url)` constructor. Open is the filled button.
- **The Card's second line,** built from Traccia's response, not from Parla's request: `Sun 18/10 · 15:00`, `Today · no reminder` or `Anytime · no reminder`.
- **`card::act`:**
  - Open runs the existing `DefaultBrowser`, only if the url starts with the configured base URL.
  - Undo calls `traccia::undo(id)` (`DELETE`, 5 s) off the UI thread:
    - on success the Card closes and the log says `Parla card: reminder undone`;
    - on failure the usual notification appears and the Card stays.
- **The outcome:** `Outcome::Reminded` opens the Card above the check, replacing slice 2's pill-only result.
- **The 5-second confirmation (Q6):**
  - a ring beside the tag spins, then fills its circle over about 5 s;
  - when it completes, the Card fades and slides away (about 300 ms);
  - hovering does not pause the countdown: a plain 5 s timer (18/10 amendment, spec Decisions Q6);
  - a generation-checked close, so a late timer can't close the next session's Card;
  - under reduced motion, no animation: a plain 5 s wait, then the Card disappears.

**Tests:**
- the Card construction and its buttons;
- the auto-close only closes its own generation;
- the Open url guard, which refuses another host;
- Undo with a fake client, on success and on failure (the Card stays);
- a history row is still written once, as `created`.

**Done when:** Mattia says a reminder, sees "Chiamare Luca · Sun 18/10 · 15:00", Undo removes it from `/personal` (it shows in the Trash), and Open lands on the To-do's panel.

## Slice 5: end to end with Telegram (waits on Traccia)

**Blocked outside Parla:**
- Traccia step 08, the Reminder scheduler and Telegram delivery (TRC-158);
- step 15, the production rollout and creating the `parla` token (TRC-165).

**Do:** with the production URL and the `parla` token in Settings, Mattia says "ricordami tra due minuti di provare Parla". The Telegram message arrives at that minute, and its link opens the panel. Also check that a reminder that was undone never arrives.

**Then:** update the README (the Reminders section, Settings and data paths, but no token) and close PARLA-82.
