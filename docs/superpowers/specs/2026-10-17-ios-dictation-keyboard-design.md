# Parla iOS personal dictation keyboard

Date: 2026-10-17. Status: agreed product scope; proposed implementation awaiting
physical-device validation. This document is the complete deliverable of this
PR. Implementation happens on Mattia's MacBook, not this Linux machine.

## Purpose and agreed behavior

Mattia wants to dictate without watching words appear. He opens Parla once
before a roughly 45-minute dog walk, returns to whichever app he wants to write
in, selects the Parla keyboard, taps Start, speaks, and taps Stop. Parla inserts
the complete final transcript. Repeated dictations during the prepared session
must not require switching to Parla and back each time. Avoiding that repeated
detour is a primary goal, based on his experience with Wispr Flow on iOS.

The target is Mattia's **iPhone 17 Pro Max running iOS 27**, as reported by him.
Verify the precise device OS build and matching Xcode/SDK on the MacBook. Do not
infer new platform permissions from the OS version.

Agreed requirements:

- Personal installation from the MacBook; no App Store release required.
- Apple native on-device transcription is the first prototype's provider.
- Italian and English; evaluate mixed Italian/English speech explicitly.
- Never show or insert partial transcription. A level meter and status are fine.
- Internal incremental processing is allowed to reduce Stop-to-insertion time.
- Insert raw recognition output, including the recognizer's punctuation, with
  no separate LLM cleanup, translation, or rewriting.
- Keep the microphone session active between utterances until explicit End
  session. No application inactivity timeout. Idle audio is discarded.
- Stop ends an utterance, not the microphone session. Cancel discards the
  current utterance and returns to readiness.
- If the original keyboard interaction is lost, retain the completed text for
  explicit Insert instead of automatically inserting into another destination.
- Keep the latest failed recording temporarily for Retry or Discard.
- Calls, process termination, and other iOS interruptions may require manually
  reopening Parla and preparing the session again.

These requirements express the desired experience; the feasibility gates below
determine whether public iOS APIs can deliver it reliably.

## Terms

- **Session:** the period during which Parla is prepared for repeated dictations,
  from explicit activation until End session or interruption.
- **Utterance:** speech intentionally captured between Start and Stop, or
  discarded by Cancel.
- **Final transcript:** the completed recognition result for one utterance.
- **Pending result:** a final transcript held for an explicit Insert action.
- **Recovery recording:** the latest failed utterance's audio retained for Retry.

## Platform facts and boundaries

Apple documents that custom keyboard extensions cannot access the microphone.
Full Access allows additional capabilities such as shared-container writes and
networking; it does not grant microphone access. The companion application must
own recording. See [keyboard restrictions][keyboard], [extension restrictions][extensions],
and [open access configuration][open-access].

An application can continue audio recording in the background with an appropriate
audio session and the audio background mode. This supports the proposed approach,
but is not a guarantee that the entire keyboard-to-companion workflow will work
for 45 minutes. See [record audio sessions][record]. Keeping capture active is
different from merely keeping an audio-session object allocated: do not claim
that the microphone is paused while using active capture to stay available.

Shared storage is an exchange mechanism, not permission to wake a suspended or
terminated app. Do not depend on it to cold-start Parla. The documented support
for [NSExtensionContext.open][open-url] is extension-point-specific and does not
establish custom-keyboard support. Automatic launch of the companion or return
to an arbitrary originating app is not a prerequisite or a promised feature.

A keyboard inserts text using `textDocumentProxy.insertText`, rather than the
clipboard. It cannot run in secure fields, some phone-pad fields, or apps that
reject custom keyboards. No claim of support for every iPhone text field is made.
See [Apple's keyboard guide][keyboard].

Apple introduced SpeechAnalyzer and SpeechTranscriber in iOS 26. They provide
public on-device speech transcription; this does not prove parity with iOS 27's
system keyboard dictation, including its language switching. Model assets,
supported hardware, and supported locales must be checked on the actual device.
See [Apple's introduction][speech-session] and [SpeechTranscriber][transcriber].

## Proposed implementation

The following are implementation recommendations, not additional user decisions.
Keep them adjustable in response to the device experiments.

### Native companion and keyboard

Create a separate native Swift iOS project, suggested location `ios/`, with a
SwiftUI companion and a custom keyboard extension using `UIInputViewController`.
This is a new iOS subsystem. It does not replace or modify the existing Windows
Tauri/Rust engine, `transcribe-rs`, or its native dependencies.

The companion owns permissions, asset preparation, the audio engine, speech
recognition, session lifecycle, and recovery audio. The keyboard owns Start,
Stop, Cancel, compact status, pending Insert, recovery controls, and the globe
control for switching keyboards. Provide End session in the companion and, if
command delivery is working, the keyboard. No full typing keyboard, live words,
transcript history, account system, or server is required.

Initial setup asks for the permissions the chosen APIs actually require,
explains keyboard enablement and Full Access, prepares speech assets, and only
then exposes Start session. Missing access, unsupported locale/hardware, or
missing assets must produce a specific actionable state rather than false Ready.

### Session and utterance flow

Session state is inactive, preparing, armed, or interrupted. An armed session has
an utterance state of idle, recording, transcribing, pending result, or failed.
These are separate state machines: a result can remain pending after its audio
session has ended.

1. In the foreground, Start session opens capture and publishes a new session ID.
   Ready is shown only after capture and recognition prerequisites are ready.
2. The user manually returns to the target app. While idle, capture remains
   active but its buffers are neither saved nor sent to recognition or a server.
3. Start creates an utterance ID and captures subsequent buffers. Show Starting
   until the companion acknowledges, then Recording. Preserve the first syllable
   by minimizing command latency; do not compensate by retaining idle audio.
4. Feed utterance audio to the native transcriber incrementally where useful,
   hiding all intermediate results. Keep a bounded copy for failure recovery.
5. Stop closes the utterance input, finalizes recognition, and publishes one
   final result. The session remains armed. New Start is disabled during
   transcription or while a result/recovery decision is unresolved.
6. Insert automatically only when continuity checks still pass. Otherwise show
   Text ready with Insert and Discard, without previewing the transcript.
7. Cancel invalidates the utterance, deletes its audio, ignores late recognition
   callbacks, and returns the armed session to idle.

End session invalidates in-flight work and commands, releases microphone capture,
and prevents late automatic insertion. Retain an already completed pending text
result for explicit Insert or Discard; delete recovery and in-flight audio.
Locking must never initiate a dictation. The prototype must establish whether
an already armed session survives lock/unlock; after unlocking, read real state.

### Communication and insertion

Use an App Group shared container with a small versioned protocol. Both targets
must have the same App Group entitlement. Commands carry session ID, utterance
ID, command ID, and command kind; responses include acknowledgments, current
state, heartbeat, and a result ID. Publish records atomically, use one writer per
record, and make command handling idempotent. Reject stale session IDs and
late or duplicated Stop/Cancel/result messages.

Start with bounded polling while the keyboard is visible and the companion's
capture session is active. Measure its latency and battery impact. Notifications
may be a hint only if supported for this use; persisted state remains authoritative.
Do not assume undocumented mechanisms or shared defaults provide reliable wakeup.
There is no polling while inactive, and queues and audio buffers must be bounded.

Use a keyboard interaction generation alongside session/utterance IDs. Invalidate
it on disappearance, extension recreation, or observed input-context change,
including text/selection changes during transcription. Revalidate before insertion.
UIKit does not provide a universal, durable identity for the original host field;
these checks are conservative heuristics. If continuity cannot be established
reliably, the device gate must select explicit Insert as the fallback and report
that limitation rather than claim destination safety.

Claim a result before calling `insertText` and never automatically replay it on
recreation. The API has no transactional insertion acknowledgment: a crash can
leave insertion outcome unknown. Mark such a result uncertain, require explicit
user action, and warn that reinserting may duplicate text. Do not claim exactly
once insertion across process failure. Check a pending result again after keyboard
recreation before allowing a new utterance.

### Recognition and recovery defaults

Begin with SpeechAnalyzer and SpeechTranscriber. Verify available Italian and
English locales, device support, asset installation, and finalization behavior
using the current SDK. Do not assume automatic language identification or
code-switching support. First compare Italian and English configurations on the
same mixed-language samples; offer an explicit Italian/English selector if needed.
If that cannot meet Mattia's mixed-language needs, record the limitation and
evaluate a later provider instead of silently translating or dropping words.

Proposed initial bounds: at most one active utterance, 120 seconds per utterance,
and 30 seconds to finalize after Stop. At the utterance limit, stop and finalize
with visible status; at the finalization timeout, enter failed and offer Retry or
Discard. These limits apply to utterances, not the 45-minute session, and can be
tuned from measurements.

Keep only the latest recovery recording in private local app storage. Delete it
on successful recognition, Discard, replacement, End session, or after 24 hours,
whichever comes first. Purge expired/orphaned files at startup. Use platform file
protection; never log or commit recordings. Retain a pending transcript until
inserted/discarded or 24 hours, also checking expiry on startup. A retry reuses the
saved audio; after loss of interaction continuity its result requires Insert.

An interrupted recording is a failure with recoverable audio, not an automatic
successful partial dictation. Empty recognition inserts nothing. Display clear
permission, asset, interruption, and recognition errors. No automatic upload or
provider switch occurs in the Apple-only prototype. If cloud is added later,
the companion owns credentials and uploads only intentional utterance audio.
xAI is the first cloud candidate; OpenAI is another option, not part of v1 scope.

## MacBook feasibility gates and build checklist

Do the smallest native device experiment before polishing the keyboard:

1. Install an Xcode version supporting the phone's exact OS build. Create the
   containing iOS app and Custom Keyboard Extension targets with distinct bundle
   IDs and the same signing team. Enable App Groups for both, audio Background
   Modes for the companion, and the keyboard's RequestsOpenAccess configuration.
   Add the actual microphone/recognition usage descriptions required by the APIs.
2. Check signing and entitlements on the physical device. Apple's current
   [capabilities table][capabilities] lists App Groups and Background Modes for
   free Apple Developer accounts; verify provisioning in practice rather than
   assuming paid membership is required. Personal Team profiles expire after
   seven days, so a free installation needs periodic rebuilding/reinstallation.
   See [Apple's account overview][account].
3. Pair/trust the iPhone with the Mac, enable Developer Mode when prompted, and
   run the companion on the iPhone from Xcode. Grant permissions, download speech
   assets, verify Italian/English availability, and enable Parla under Settings
   > General > Keyboard > Keyboards, including Allow Full Access. Test that missing
   access is explained correctly. Simulator success is insufficient.
4. Arm capture in the companion, manually return to a test text field, and prove
   that keyboard commands are acknowledged while the companion is backgrounded.
   Repeat Start/Stop and Cancel without opening the companion between utterances.
   Verify recognition and final-result delivery while backgrounded too.
5. Verify lock/unlock, keyboard dismissal/recreation, another text field/app,
   companion termination, calls/Siri/audio interruptions, and recovery. Stale
   heartbeat means unavailable, not Ready. Reopening and rearming is acceptable
   after a genuine interruption; repeated rearming in normal use fails the goal.
6. Perform the full walk test below. If continued capture or command delivery is
   unreliable, stop and report the evidence. A per-utterance app detour may be
   documented as a fallback, but it does not satisfy the primary acceptance case.

No supported cold-launch shortcut or automatic return path is assumed. Explore
one only as an optional improvement after the core session workflow works.

## Acceptance and measurement

Run on the actual iPhone, with Italian, English, and mixed speech. The main test
is a roughly 45-minute armed session with repeated short dictations, idle gaps,
normal walking/locking/unlocking, and multiple supported target apps. Record the
number and reasons for required reactivations. Continuous background readiness
for this scenario is a goal, not a claim established by this specification.

Verify all of the following:

- No partial words appear in the keyboard or destination before finalization.
- A normal Start/Stop produces one complete raw transcript without app switching.
- The first syllable is captured when speaking immediately after Recording; also
  measure behavior when speaking immediately after tapping Start.
- Cancel inserts nothing, and late callbacks cannot insert cancelled text.
- End session releases the microphone and blocks late automatic insertion.
- Switching field/app, dismissing the keyboard, and process recreation do not
  automatically insert stale text; uncertain results remain explicit actions.
- Failures preserve the latest audio for Retry/Discard according to retention
  policy. No idle audio is retained or submitted for recognition.
- Locking does not start speech capture as an utterance; genuine interruptions
  lead to accurate unavailable state and successful manual recovery.
- Denied permissions, missing Full Access/assets, unsupported locales, empty
  speech, long utterances, and timeouts produce usable states without hanging.
- The globe control returns to Apple's keyboard; unsupported secure fields and
  apps are recognized as platform boundaries.

Collect monotonic timestamps for Start tap, capture acknowledgment, Stop tap,
recognition finalization, result publication, and insertion attempt. Report
median and p95 Stop-to-insertion-attempt latency, sample counts, capture-start
delay, missed first syllables, losses, duplicate attempts, and reactivations.
The insertion attempt timestamp is not proof that the host displayed the text;
check actual field contents manually. Measure session battery use, thermal
behavior, and effects on other audio. Keep logs to metadata, without audio/text.
There is no invented latency target or battery guarantee: establish a baseline
and assess whether it improves Mattia's experience.

## Deferred work

Cloud providers and cost comparisons, other local engines, full typing layout,
transcript history, desktop synchronization, App Store distribution, automatic
app switching, and Action Button/Shortcuts integrations are outside this first
prototype. Native on-device transcription is already local; a later local-engine
experiment would be an alternative to Apple's engine, not a prerequisite.

[keyboard]: https://developer.apple.com/library/archive/documentation/General/Conceptual/ExtensibilityPG/CustomKeyboard.html
[extensions]: https://developer.apple.com/library/archive/documentation/General/Conceptual/ExtensibilityPG/ExtensionOverview.html
[open-access]: https://developer.apple.com/documentation/uikit/configuring-open-access-for-a-custom-keyboard
[record]: https://developer.apple.com/documentation/avfaudio/avaudiosession/category-swift.struct/record
[open-url]: https://developer.apple.com/documentation/foundation/nsextensioncontext/open%28_%3Acompletionhandler%3A%29
[speech-session]: https://developer.apple.com/videos/play/wwdc2025/277/
[transcriber]: https://developer.apple.com/documentation/speech/speechtranscriber
[capabilities]: https://developer.apple.com/help/account/reference/supported-capabilities-ios
[account]: https://developer.apple.com/help/account/basics/about-your-developer-account
