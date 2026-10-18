# Command mode: the `create_reminder` tool (PARLA-82)

Design and research, 2026-10-17. **Not built.** Mattia answered the open questions the same evening: see **Decisions** at the end. Tickets: sub-issues of PARLA-82, in Backlog.

Vocabulary: `GLOSSARY.md` (To-do, Reminder and Project reference are new). Decisions:
- ADR-0001: command mode is the only LLM path, and dictation stays raw.
- ADR-0002: one LLM agent with allowlisted tools.
- ADR-0003: command replies preview in the Card.
- ADR-0004 (new): command tools may call Mattia's own services over the tailnet, and fail rather than queue.

Traccia side:
- `TRACCIA/traccia/docs/personal-space.md` and `docs/rest-todos.md`, read only;
- the PARLA-82 comments (17/10 contract);
- milestone P11 Personal Space.

## Problem Statement

Mattia thinks of things to do while he works: "call Luca tomorrow at 3", "send the quote to Cittadellese on Thursday". Today, writing one down means leaving the app he is in, opening Traccia, and typing the To-do and its time by hand. He already holds a chord to talk to command mode from any app. He wants to say "ricordami di chiamare Luca domani alle 15" and have it land in his Traccia Personal Space as a To-do with a Reminder, then get pinged on Telegram at that time. The usual rules apply: nothing touches dictation, and command mode stays lean.

## Solution

`create_reminder` becomes the second tool on command mode's allowlist, after `open_url`. When the instruction asks to be reminded of something, the LLM calls the tool in the same single round trip, instead of replying with text. It passes the To-do's title, any `@KEY` Project references, the day and the time. The day and time are resolved from "Now" in the request.

Parla then makes one REST call over the tailnet, `POST /v1/todos`, using a `todos`-scoped token. Traccia stores the To-do and later sends the Reminder on Telegram. That delivery is Traccia's step 08 (TRC-158), still being built: until it lands, Reminders are stored but nothing pings.

The Card confirms what Traccia actually stored, for example "Chiamare Luca · Sun 18/10 · 15:00". It offers **Open** (the To-do in the dashboard) and **Undo** (`DELETE`, a soft delete). If Traccia can't be reached, the run fails loudly: the red pill and a notification. Nothing is queued.

The tool sits behind its own toggle, off by default. While it's off, the request, the prompt and the eval are byte-identical to today.

## Research findings

### 1. Resolving relative dates: the model gives the local day and time; Rust adds the offset

The brief proposed that the model emit `remindAt` with an offset. A probe on `gpt-6-luna` showed a better split. The probe ran 14 cases plus 9 edge cases, in IT and EN, 3× each at efforts `low` and `none`. It reused the real system prompt and strict schemas, with the Now set to Saturday 2026-10-17 21:14 or Monday 2026-10-19 10:00. About 400 calls in total, roughly $0.05.

| Variant | Tool arguments | `low` | `none` |
|---|---|---|---|
| A | `title`, `remind_at` (date-time with offset, nullable) | 42/42 | 42/42 |
| B | `title`, `day` (date, nullable), `time` (`HH:MM`, nullable) | 42/42 | 39/42 |
| B + a 15-day calendar in the request | same | 40/42 | 40/42 |

- **gpt-6-luna does the date arithmetic well**: "domani alle 15", "next Monday morning", "tra due ore", "fra 10 minuti", "giovedì prossimo alle 9", "il 3 novembre", "il 2 alle 9" (it reads this as 2 November), "dopodomani a mezzogiorno", and "lunedì" said on a Monday (in 7 days). A calendar table in the prompt didn't help, so leave it out.
- **Variant A got the DST offsets right only because the prompt spelled out the switch** ("+02:00 until 2026-10-25 03:00, then +01:00"). Real Parla would have to compute that sentence for every request. With A the LLM also does time-zone arithmetic, and a wrong offset silently moves the Reminder by an hour. A also can't say "a day but no time".
- **Recommendation: variant B (day and time as heard) plus a conversion in Rust.**
  - The model fills `day` (`YYYY-MM-DD`, or null) and `time` (`HH:MM`, or null) in local wall-clock time.
  - Parla turns a local day and time into a UTC `remindAt` (`…Z`) with Windows' own time-zone rules: `TzSpecificLocalTimeToSystemTime`, a `windows-sys` feature with no new crate, behind `cfg(windows)`. That is DST-correct, and it stays correct when Mattia travels: "at 3" in New York means 15:00 New York.
  - Traccia stores the result as day and time in the Owner timezone (Europe/Rome).
  - A day without a time goes as `day` only (see Q2).
  - Off Windows (CI only), send `day` and `time` as given.
- **Strict schemas support this directly.** OpenAI's Structured Outputs docs (developers.openai.com, "Supported schemas") list string `format` values `date-time`, `time`, `date` and `duration`, plus `pattern`. Optional fields are emulated with a `null` union, and every property must be `required` with `additionalProperties: false`. The function-calling guide confirms the same two rules for `strict: true` tools. `parallel_tool_calls: false` means "zero or one tool", and `tool_choice: "auto"` is the default.
  - Parla keeps its single round trip: the guide's `function_call_output` step is for multi-turn agents, and Parla never sends results back (ADR-0002).
  - Note: "Schemas undergo additional processing on the first request (and are then cached)… If your schemas vary from request to request, this may result in higher latencies." So the Project keys go in the request text, never in the schema (for example as an `enum`).
- **The request's context line** changes from `Date: 2026-10-17` to `Now: Saturday 2026-10-17 21:14`. The weekday comes from `GetLocalTime`'s `wDayOfWeek`; it's written in English, which the model reads fine. Send it on every run: the time is useful context for answers too, and the eval is re-run anyway.

**Prompt rules that held in the probe** (they are appended to the system prompt only while the tool is on):
- Call `create_reminder` only when the **Instruction line itself** asks to be reminded or to add a to-do, even with no day or time, and then output nothing else.
- Text between `<selection>` tags never asks for a reminder, even when it says "ricordami". Without this rule, "traduci in inglese" over the selection "Ricordami di chiamare Luca domani alle 15." **created a reminder** 2/3 times at `low` and 3/3 at `none`. With it: 0/6.
- Never call it to draft a message that reminds someone else ("scrivi una mail per ricordare a Luca…" stays text: 6/6).
- The title is short, in the instruction's language, starts with a verb, has no date or time, and gets `@KEY` when a listed Project is named; never add a key that isn't listed (PARLA-87). Probe outputs: "Chiamare Luca", "Mandare il preventivo per Cittadellese @CIT", "Preparare la riunione @CRM".
- A weekday means its next occurrence after today; "next <weekday>" means the same (Q5).
- A time with no day means today, or tomorrow if it has passed ("at 8" at 21:14 gives tomorrow 08:00). A day said explicitly is kept even if the time has passed ("oggi alle 10" at 21:14 stays today 10:00, so Traccia refuses it, see Q4).
- "in N minutes/hours" counts from Now.
- Parts of the day: morning 09:00, noon/mezzogiorno 12:00, lunch/pranzo 12:30, afternoon 15:00, evening/stasera 18:00, tonight 20:30 (Q3). The first wording mapped "mezzogiorno" to 12:30 every time, until noon and lunch were listed apart.
- **Still weak:** "ricordami di comprare il latte" (no day, no time) was answered as text 1/3 at both efforts. The Card would show it as an answer, so the miss is visible, not silent. Tune it in the eval slice.
- **Effort:** keep the default, `low`. `none` matched it on the final prompt, but it was worse on the undated case (3/3 misses before the wording fix).

### 2. Vague cases

These are decided in Rust, deterministically, after the model has said what it heard. That makes them unit-testable offline.

| What was said | Model gives | Parla sends | Card says |
|---|---|---|---|
| day and time, future | day, time | `{title, remindAt}` | `Sun 18/10 · 15:00` |
| day only ("lunedì") | day, null | `{title, remindAt: day 09:00}` if that is still in the future, else `{title, day}` (**Q2**) | `Mon 19/10 · 09:00`, or `Today · no reminder` |
| nothing ("ricordami di comprare il latte") | null, null | `{title}`: an Anytime To-do (**Q1b**) | `Anytime · no reminder` |
| a moment already past ("oggi alle 10" at 21:14, "stasera" after 18:00) | day, time | the request; Traccia answers 400 `details.fields.remindAt` | no Card: a red pill, "15:00 today has already passed" (**Q4**) |
| "lunedì" on a Monday | next Monday | as above | the Card shows the date, so a surprise is visible |

The 400 is Traccia's rule, so Parla doesn't duplicate the past check. One code path, one source of truth, and Parla's clock can't disagree with it.

### 3. Project references

- **What:** `GET /v1/projects?limit=250` (a `todos` token may read it). Keep the `active` and `paused` Projects, the same set Traccia's own `@` autocomplete offers, and render one `KEY: Name` line each under a `Projects in Traccia (KEY: Name):` heading in the request text (PARLA-87; the heading was `Projects:` in this design). Traccia matches `@KEY` exactly and case-insensitively, and an unknown key stays plain text, so a wrong guess never fails a request.
- **When:** no fetch at startup, and nothing on the dictation path.
  - At the **confirmed command hold**, the same moment as `openai::prewarm`, a background thread refreshes the list if it is empty or older than **10 minutes**. That only happens while the reminder tool is on.
  - The fetch has a 2 s timeout and runs while Mattia is still speaking, so it is normally done before the LLM request is built.
  - The run uses whatever is cached. Only when the cache is empty and a fetch is in flight does it wait, for at most 500 ms.
  - A failed fetch keeps the last list. With no list at all, the request goes out without the Projects block, and the To-do is created without `@KEY`.
  - The cache lives in memory only: when Traccia is unreachable the POST fails anyway, so a list on disk would buy nothing.
- **Bonus:** the fetch opens TCP and TLS to Traccia in the shared client's pool, so the POST skips the handshake.
- **Cost:** about 8 tokens per Project, so about 240 tokens for 30 Projects, or about $0.00002 per run.

### 4. Confirmation and Undo in the Card: create first, offer Undo (Q1, decided)

ADR-0003 previews **text** because a rewrite overwrites something Mattia hasn't seen. It says tools act at once, "because an action has nothing to preview". A reminder is an action, and it's cheap to reverse:
- Traccia's delete is soft and restorable from the Trash;
- nothing pings until the Reminder's moment, and a deleted To-do is never sent;
- a `todos` token can't purge.

**Decided: create at once, then confirm in the Card with Undo.**
- After a 201, the pill shows the check and the Card opens with:
  - **Tag:** an orange dot, "Reminder".
  - **Header:** the instruction, quoted, as for command replies.
  - **Body:** the To-do's `title` as Traccia stored it (`@KEY` included), with a second line built from the response's `day` and `reminder.time`, e.g. `Sun 18/10 · 15:00`, `Mon 19/10 · 09:00`, `Today · no reminder` or `Anytime · no reminder`. It shows what Traccia stored, not what Parla meant, so an hour lost to DST would be visible.
  - **Buttons:**
    - **Open** is the filled one: it opens the To-do's `url` in the default browser through the existing opener. It only opens if the `url` starts with the configured base URL.
    - **Undo** calls `DELETE /v1/todos/{id}`. On success the Card closes and the log says `Parla card: reminder undone`. On failure it shows the usual error notification and the Card stays, so Undo or Open can be retried.
    - **✕** dismisses.
- **Lifetime (Q6, decided): a 5-second confirmation that closes itself.** This is the one Card with a timer.
  - A ring next to the tag spins while the Card opens, then fills its circle over about 5 s like a countdown.
  - When the circle completes, the Card fades and slides away gently (about 300 ms), and the pill's check goes with it.
  - Hovering does **not** pause the countdown: a plain 5 s timer (decided 18/10, see Decisions).
  - A click (Open, Undo, ✕) or the next session closes it earlier, as for every Card. An Undo stops the countdown while its `DELETE` runs; if it fails, the Card stays without a countdown (PARLA-88).
  - Under reduced motion: no spin, a plain 5 s wait, then the Card simply disappears.
- **History:** append-only, as decided for the Card. The row says `created`. An Undo goes to the log only.

**Why not preview and Confirm:**
- The common case would cost a click.
- Worse, a forgotten Card (Mattia talks, then turns back to his work) would mean **no reminder at all**, a silent failure, and the next dictation closes the Card.
- With create-first, an ignored Card is the success path. Undo covers the rare misheard date, and the Card shows the date in plain view.

**Card changes:**
- `Tag::Reminder`, and `Action::Open` and `Action::Undo`.
- The Card keeps the To-do's `id` and `url` with its generation, so `card::act` can run them.
- The Card's text stays selectable, and Copy isn't offered: there is nothing to paste.
- The countdown ring and the auto-close (in `overlay.ts`, `overlay.css`, and a generation-checked close in `overlay.rs`), so a late timer never closes the next session's Card.

### 5. Failure modes

| Case | Detected as | Pill and notification | History |
|---|---|---|---|
| Tailscale down, DNS fails (the MagicDNS name doesn't resolve) | a transport error, in milliseconds | red ✕, "Traccia can't be reached. Is Tailscale on?" | `failed` + error |
| Server or tailnet unreachable | connect timeout (5 s, the shared client's) | same | same |
| Server hangs | request timeout of **5 s** on the POST (the LLM part keeps its own 20 s) | "Traccia didn't answer in 5 s." | same |
| 400 `validation_error`, field `remindAt`, `time` or `day` | status + `details.fields` | "15:00 today has already passed." (the time comes from Parla's request) | same |
| 400, another field (empty or long title) | | "Traccia refused the reminder (title)." | same |
| 401 | | "Traccia rejected the token. Check Settings → Reminders." | same |
| 403 | | "This Traccia token can't create To-dos (it needs the `todos` scope)." | same |
| 5xx | | "Traccia error (HTTP 502)." | same |
| The model's arguments don't validate (bad date or time, empty title) | the validator, before any request | "The reminder could not be read." | same |

- **No queue and no retry:** decided by Mattia, ADR-0004. The instruction is in history, so he can say it again.
- **Esc** during the LLM wait discards the reply and creates nothing, as for `open_url`.
- Once the POST has started, it is not cancelled. A To-do created after Esc would surprise Mattia, so the run ignores Esc from the moment the POST starts; the window is about 5 s at worst, normally tens of milliseconds.
- **Error bodies:** keep only `code` and the `details.fields` names, never the message (the same rule as the Responses client). Never log the token or the URL with it. `provider::redact_secrets` already covers bearer strings.
- **No new dependency:** `reqwest` blocking with rustls is already there, and so is the shared `openai::client()`. Its pool and 5 s connect timeout suit a second host. Rename it to a neutral `http::client()` only if it reads better.

### 6. Settings and secrets

- **Toggle:** `command.reminders: bool`, default false, in the Settings "Command mode" card ("Reminders in Traccia"). It needs command mode on, a base URL and a token. Without them it is disabled with a hint, as the Enabled toggle is without an OpenAI key.
- **Connection:** `providers.traccia = { "base_url": "", "token": "" }`, next to the OpenAI and xAI keys, so it gets their plain-JSON storage and any secret handling those keys get (the old-settings loader already keeps unknown sections).
  - The base URL is stored without a trailing slash, and `http` or `https` is accepted (the tailnet encrypts either).
  - Placeholder in the UI: `https://<traccia-host>.<tailnet>.ts.net`.
  - The token field is a password input, and the token is never written to the log.
  - Env overrides `TRACCIA_URL` and `TRACCIA_TOKEN` are for the live test only, like `OPENAI_API_KEY`.
- **Test connection:**
  - `GET /v1/me` → `{actor, tokenName}`, then `GET /v1/projects?limit=250` for the count. It shows "Connected as you · token parla · 12 projects".
  - It warns if `actor` isn't `you` (the To-dos would be stamped as an agent's).
  - The errors are those of §5.
  - `/v1/me` doesn't report the scope, so the hint says to create the token with `traccia token create --name parla --actor you --scope todos`. An `all` token would work but can do far more than Parla needs.

### 7. Latency and cost

- **Dictation: zero.**
  - No code runs on the dictation path.
  - The Projects fetch happens only at a confirmed command hold, and only with the tool on.
  - The hook is untouched.
- **Command runs with the tool off:** byte-identical requests, so no change.
- **Command runs with the tool on:**
  - **No extra LLM round trip:** the call comes back in the same response that would otherwise carry text.
  - **Input:** 477 tokens today for a plain run. The probe's request was about 965 with 4 Projects; that is the tool's schema plus about 200 tokens of rules. With 30 Projects, about 1,200 in total. At $0.10 per million, that adds about $0.00007 per run.
  - **Output:** about 36–47 tokens for the call.
  - **The POST:** one request to Traccia on a pool connection already warm from the Projects fetch. That's tens of milliseconds on a direct tailnet path, and longer if Tailscale relays through DERP. Mattia's machine lock was not held, so this design takes no timed measurements; the eval slice records them.
  - **Reply time:** the rules add input tokens, which may add a little time to the reply. The eval slice re-measures the median and p90, and compares them with the current ~1.27 s median.

## User Stories

1. As Mattia, I want to hold the command chord and say "ricordami di chiamare Luca domani alle 15", so that a To-do with a 15:00 Reminder tomorrow lands in my Personal Space without leaving my app.
2. As Mattia, I want English instructions ("remind me next Monday morning to send the invoice") to work the same, so that I can use whichever language comes out.
3. As Mattia, I want relative times ("tra due ore", "fra 10 minuti", "dopodomani a mezzogiorno") resolved from the current local time, so that I never say a full date.
4. As Mattia, I want "per Cittadellese" or "progetto CRM" to become `@CIT` or `@CRM` in the title, so that the To-do shows under that Project.
5. As Mattia, I want the Card to show the title, day and time Traccia actually stored, so that a misheard date is obvious.
6. As Mattia, I want an Undo on the Card, so that a wrong reminder disappears with one click.
7. As Mattia, I want an Open button, so that I can add details in the dashboard.
8. As Mattia, I want a clear red failure when Traccia or Tailscale is down, so that I never believe a reminder was set when it wasn't.
9. As Mattia, I want "translate this" over a selection that says "ricordami…" to translate, so that a reminder is only made when I ask for one.
10. As Mattia, I want "write an email reminding Luca…" to draft the email, so that reminders for other people stay text.
11. As Mattia, I want the tool off by default behind its own toggle, so that command mode doesn't change until I set Traccia up.
12. As Mattia, I want a Test connection button, so that I know the URL and token work before I rely on them.
13. As Mattia, I want my Traccia token stored like my other keys and never logged, so that it doesn't leak.
14. As Mattia, I want dictation to stay exactly as fast as it is, so that this feature costs me nothing when I'm not using it.
15. As Mattia, I want the reminder run recorded in history with its title, day and time, so that I can see what was created.

## Implementation Decisions

**Product boundary**
- Command mode only (ADR-0001), as one more allowlisted tool (ADR-0002) that calls Traccia over the tailnet (ADR-0004). It runs in the single round trip, and its result never goes back to the model.

**Modules**
- **`command_mode/traccia.rs` (new, deep):** the Traccia client behind a narrow `Reminders` port:
  - `create(title, when) -> Created {id, url, title, day, time}`;
  - `undo(id)`;
  - `projects() -> Vec<(key, name)>`;
  - `me() -> (actor, token_name)`.

  It holds pure request building and response or error parsing (fixture-tested), plus thin blocking calls on the shared client with explicit timeouts. `When` is `Moment(utc)`, `Day(date)` or `Anytime`.
- **`command_mode/tools.rs`:**
  - `Tool::CreateReminder {title, day, time}`, with its strict declaration: `title` string, `day` `["string","null"]` with format `date`, and `time` `["string","null"]` with pattern `^([01][0-9]|2[0-3]):[0-5][0-9]$`.
  - The validator: a non-empty title of at most 500 characters, a real date, a real time.
  - `declarations(reminders_on)` adds the tool only while it's on.
- **`command_mode/when.rs` (new, pure):** the §2 table. It takes `(day, time, now, default_time)` and returns a `When`, through a `LocalClock` port:
  - on Windows, `GetLocalTime` plus `TzSpecificLocalTimeToSystemTime` for local to UTC;
  - in tests, a fake.

  The off-Windows build sends `day` and `time`.
- **`command_mode/responses.rs`:**
  - `SYSTEM_PROMPT` stays as it is.
  - `REMINDER_RULES` is appended only while the tool is on.
  - `user_text` writes `Now:` instead of `Date:`, then `Projects in Traccia (KEY: Name):` when there is a list.
- **`command_mode/mod.rs`:**
  - `run_tool` dispatches `CreateReminder`: resolve `When`, call `Reminders::create`, record the row, and return `Outcome::Reminded(Card)`.
  - `Ports` gains `reminders: Option<&dyn Reminders>`, which is None while the tool is off.
  - `Run.date` becomes `Run.now`.
- **`projects cache` (in `traccia.rs`):** `Mutex<Option<(Instant, Vec<…>)>>` plus an in-flight flag. `refresh_if_stale()` is called from `flow.rs` at the confirmed command hold, next to `openai::prewarm`.
- **`card.rs` / `overlay.ts`:** `Tag::Reminder`, `Action::Open` and `Action::Undo`, and the Card carries the reminder's `id` and `url`. The second line is muted.
- **`timing.rs` history:**
  - `CommandOutcome::Created`;
  - `tool: "create_reminder"`;
  - `url` set to the To-do's url;
  - `reply` set to `"<title> · <day> <time>"`, so the existing Copy in the main list still makes sense.
- **Settings (`settings.rs`, `settings.ts`):**
  - `command.reminders`, and the `providers.traccia` section;
  - a Tauri command `test_traccia` that returns the §6 line or the §5 error;
  - `save_settings` refuses `reminders: true` without a URL and a token.

**Not changed:** the hook, the recorder, `flow.rs`'s dictation path, the inserter, the Safety Net, and `open_url`.

## Testing Decisions

- **`when.rs`** (pure), each row of §2:
  - day and time;
  - a day only, with the default time still ahead and already past;
  - nothing;
  - the DST edges through the fake clock: 2026-10-25 02:30, which happens twice in Europe/Rome, and a time that doesn't exist on 2027-03-28. Expect what Traccia expects: the first occurrence, and the hour after.
- **`tools.rs`:** the declaration is strict and byte-pinned; the validator accepts and refuses dates, times and titles; an unknown tool is still refused; the tool is declared only while it's on.
- **`traccia.rs`:**
  - **Requests:** the create body with `remindAt`, `day` or neither; the bearer header is marked sensitive; the base URL is joined without a double slash.
  - **Parse fixtures:** a 201 To-do, 400 with `details.fields.remindAt`, 400 for the title, 401, 403, 5xx, a body that isn't JSON. The error text never includes the server's message.
  - **Projects:** filtered to active and paused, with `nextCursor` ignored at 250.
- **Orchestrator** (fake ports):
  - a reminder call creates one To-do and opens the reminder Card, with Open and Undo;
  - the history row is `created`;
  - the clipboard is given back;
  - Esc before the POST creates nothing;
  - unreachable, 400, 401 and 403 fail with the §5 messages and no Card;
  - with the tool off, a stray `create_reminder` call is refused as an unknown tool;
  - the request carries `Now:` and the Projects block only when expected.
- **Responses client:** fixtures for the tool on and off (the off request is byte-identical to today's), and for `Now:`.
- **Live eval (`tests/command_eval.rs`):** add the probe's cases. Today it has 31 cases, plus 14 reminder cases from this probe and 4 controls:
  - selection says "ricordami" → text;
  - draft a reminder email → text;
  - an undated reminder;
  - a Monday on a Monday.

  The grader checks the day, the time and the `@KEY` against a fixed fake Now. The eval passes `now` in the request, so no real clock is involved. Run 3× at `low` and `none`; record the median and p90 under the machine lock.
- **Live Traccia test** (`#[ignore]`, gated on `TRACCIA_URL` and `TRACCIA_TOKEN`): create a To-do 2 minutes ahead, check the 201 shape, then `DELETE` it. It must never run in CI.
- **Hand checks by Mattia:**
  - say three reminders (IT, EN, with a Project), and check them in `/personal`;
  - Undo removes one;
  - Open lands on the panel;
  - turn Tailscale off: the red pill appears quickly;
  - the first syllable is still captured; AltGr typing works; Esc passes through when idle. Nothing in the hook changes, but command-mode runs were touched.

## Out of Scope

- Editing a To-do from the Card (changing the time, the title or the Project). Use Open.
- Lists, completing or querying To-dos by voice ("what do I have today?").
- Sub-to-dos, Details, recurrence, several Reminders.
- A queue or retry when Traccia is unreachable (ADR-0004).
- Delivering Reminders from Parla: Traccia sends them on Telegram.
- MCP transport.
- A local LLM-free parser for "ricordami…".
- Undo from the history list after the Card has closed.

## Decisions (Mattia, 17/10 evening)

Mattia accepted every recommendation, except Q6.

- **Q1. Create first, offer Undo** (§4).
  - **Q1b. No day and no time:** an Anytime To-do with no Reminder; the Card says so.
- **Q2. A day with no time:** a 09:00 Reminder on that day. If it's today and 09:00 has passed, a Planned day of today with no Reminder. Mattia will usually say an exact time.
- **Q3. Parts of the day:** morning 09:00, noon 12:00, lunch 12:30, afternoon 15:00, **evening/stasera 18:00**, tonight 20:30.
- **Q4. A moment already past:** fail loudly ("10:00 today has already passed"); Mattia says it again.
- **Q5. "Lunedì" and "lunedì prossimo":** both mean the next Monday after today (on a Monday, in 7 days).
- **Q6. The reminder Card:** a 5-second confirmation with a ring that spins, then fills its circle, then the Card fades away gently (§4).
  - **No hover pause (18/10, delegated by Mattia: "do whatever is simple and easiest in the code; I just need a confirmation"):** hovering does not pause the countdown. A plain 5 s timer, then the fade. A click or the next session still closes it earlier, and an Undo in flight holds it (PARLA-88).
- **Q7. Settings placement:** the toggle in the Command mode card, and the URL and token under it, stored in `providers.traccia`.

## Tickets

Sub-issues of PARLA-82, in Backlog, labelled, each blocked by the one before. Mattia has answered the open questions:

1. PARLA-85: Traccia connection: settings, token, Test connection.
2. PARLA-86: the `create_reminder` tool creates a To-do (pill only), plus the eval cases.
3. PARLA-87: Project references: the Projects cache and `@KEY` in the prompt.
4. PARLA-88: the reminder Card: Open and Undo.
5. PARLA-89: end-to-end check with Telegram, which waits on Traccia's step 08 (TRC-158) and rollout (TRC-165).

Plan: `docs/superpowers/plans/2026-10-17-command-reminder-tool.md`.
