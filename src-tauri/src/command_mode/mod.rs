//! Command mode (PARLA-61): the command run orchestrator and its Responses
//! client. The only code that calls an LLM (ADR-0001).
//!
//! The orchestrator owns a run from key-up to its outcome: the empty-instruction
//! rule, the LLM call, cancellation, the paste (the inserter restores the
//! clipboard) and the history row. It talks to the desktop, the LLM and
//! history through narrow ports, so it is tested with fakes and flow.rs
//! supplies the real ones.
pub mod responses;

use crate::{
    provider::ProviderError,
    settings::CommandSettings,
    timing::{self, CommandOutcome, DictationRecord},
};
use responses::{Reply, Request};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

/// The focused app, through the inserter. Selection reading (Ctrl+Insert,
/// PARLA-64) joins here, with the clipboard saved before it.
pub trait Desktop {
    /// Pastes `text` once and restores the user's clipboard afterwards.
    /// Returns the instant the paste was injected.
    fn paste(&self, text: &str) -> Result<Instant, String>;
}

pub trait Llm {
    /// One request, blocking. It may return early once `cancelled` is set.
    fn reply(&self, request: &Request, cancelled: &AtomicBool) -> Result<Reply, ProviderError>;
}

pub trait HistorySink {
    fn record(&self, record: DictationRecord);
}

pub struct Ports<'a> {
    pub desktop: &'a dyn Desktop,
    pub llm: &'a dyn Llm,
    pub history: &'a dyn HistorySink,
}

pub struct Run<'a> {
    pub settings: &'a CommandSettings,
    pub key_up: Instant,
    /// Set by Esc, from key-up until the reply is in.
    pub cancelled: &'a AtomicBool,
    /// The hold hit the cap: the instruction is recorded but not sent.
    pub hold_error: Option<String>,
}

/// What the pill and the notification show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An empty or silent instruction: nothing shown, no row.
    Silent,
    Pasted,
    Cancelled,
    Failed(Failure),
}

impl Outcome {
    /// The pill's check or cross; None drops the pill without one.
    pub fn pill(&self) -> Option<bool> {
        match self {
            Outcome::Silent | Outcome::Cancelled => None,
            Outcome::Pasted => Some(true),
            Outcome::Failed(_) => Some(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The instruction could not be transcribed: the provider's notification.
    Transcription(ProviderError),
    /// The LLM step or the paste: command mode's notification.
    Command(ProviderError),
}

/// One command run after key-up. `transcribe` fills `record` with the
/// instruction and returns None for an empty or silent one.
pub fn run(
    ports: &Ports,
    run: Run,
    mut record: DictationRecord,
    transcribe: impl FnOnce(&mut DictationRecord) -> Option<Result<(), ProviderError>>,
) -> Outcome {
    let cancelled = || run.cancelled.load(Ordering::Acquire);
    let outcome = match transcribe(&mut record) {
        None => return Outcome::Silent,
        // Esc aborts a live transcription, so its error is the cancel.
        Some(_) if cancelled() => Outcome::Cancelled,
        Some(Err(error)) => {
            record.error = Some(error.to_string());
            Outcome::Failed(Failure::Transcription(error))
        }
        Some(Ok(())) => reply_and_paste(ports, &run, &mut record),
    };
    if let Outcome::Failed(Failure::Command(error)) = &outcome {
        record.error = Some(crate::failure::command_message(error));
    }
    record.outcome = Some(match outcome {
        Outcome::Pasted => CommandOutcome::Pasted,
        Outcome::Cancelled => CommandOutcome::Cancelled,
        Outcome::Failed(_) => CommandOutcome::Failed,
        Outcome::Silent => unreachable!("a silent run returned above"),
    });
    ports.history.record(record);
    outcome
}

fn reply_and_paste(ports: &Ports, run: &Run, record: &mut DictationRecord) -> Outcome {
    let cancelled = || run.cancelled.load(Ordering::Acquire);
    if let Some(hold) = &run.hold_error {
        return Outcome::Failed(Failure::Command(ProviderError::Other(hold.clone())));
    }
    let request = Request {
        // A blank model field means the default (the card clears to it).
        model: match run.settings.model.trim() {
            "" => CommandSettings::default().model,
            model => model.into(),
        },
        reasoning_effort: run.settings.reasoning_effort.clone(),
        instruction: record.text.clone(),
    };
    record.llm_model = Some(request.model.clone());
    let reply = ports.llm.reply(&request, run.cancelled);
    record.llm_returned = Some(timing::elapsed_ms(run.key_up));
    let reply = match reply {
        // A reply or an error that lands after Esc is discarded.
        _ if cancelled() => return Outcome::Cancelled,
        Ok(reply) => reply,
        Err(error) => return Outcome::Failed(Failure::Command(error)),
    };
    record.tokens_in = reply.tokens_in;
    record.tokens_out = reply.tokens_out;
    if reply.text.trim().is_empty() {
        return Outcome::Failed(Failure::Command(ProviderError::Other(
            "the reply was empty.".into(),
        )));
    }
    let pasted = ports.desktop.paste(&reply.text);
    record.reply = Some(reply.text);
    match pasted {
        Ok(at) => {
            record.pasted = Some(timing::offset_ms(run.key_up, at));
            Outcome::Pasted
        }
        Err(error) => Outcome::Failed(Failure::Command(ProviderError::Other(format!(
            "could not paste the reply ({error})."
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, time::SystemTime};

    struct FakeDesktop {
        clipboard: RefCell<Option<String>>,
        pasted: RefCell<Vec<String>>,
        fail_paste: bool,
    }
    impl FakeDesktop {
        fn new(clipboard: Option<&str>) -> Self {
            Self {
                clipboard: RefCell::new(clipboard.map(Into::into)),
                pasted: RefCell::default(),
                fail_paste: false,
            }
        }
    }
    /// The inserter's contract: the clipboard is the reply while pasting,
    /// then the user's text again once the restore delay passes.
    impl Desktop for FakeDesktop {
        fn paste(&self, text: &str) -> Result<Instant, String> {
            if self.fail_paste {
                return Err("clipboard unavailable".into());
            }
            let saved = self.clipboard.replace(Some(text.into()));
            self.pasted.borrow_mut().push(text.into());
            *self.clipboard.borrow_mut() = saved;
            Ok(Instant::now())
        }
    }

    /// Answers with `reply`; with `late`, Esc is pressed while it waits.
    struct FakeLlm {
        reply: Result<Reply, ProviderError>,
        late: bool,
        requests: RefCell<Vec<Request>>,
    }
    impl FakeLlm {
        fn new(reply: Result<Reply, ProviderError>) -> Self {
            Self {
                reply,
                late: false,
                requests: RefCell::default(),
            }
        }
    }
    impl Llm for FakeLlm {
        fn reply(&self, request: &Request, cancelled: &AtomicBool) -> Result<Reply, ProviderError> {
            self.requests.borrow_mut().push(request.clone());
            if self.late {
                cancelled.store(true, Ordering::Release);
            }
            self.reply.clone()
        }
    }

    #[derive(Default)]
    struct FakeHistory(RefCell<Vec<DictationRecord>>);
    impl HistorySink for FakeHistory {
        fn record(&self, record: DictationRecord) {
            self.0.borrow_mut().push(record);
        }
    }

    fn text(reply: &str) -> Result<Reply, ProviderError> {
        Ok(Reply {
            text: reply.into(),
            tokens_in: Some(92),
            tokens_out: Some(18),
        })
    }

    struct Harness {
        desktop: FakeDesktop,
        llm: FakeLlm,
        history: FakeHistory,
        cancelled: AtomicBool,
        settings: CommandSettings,
    }
    impl Harness {
        fn new(reply: Result<Reply, ProviderError>) -> Self {
            Self {
                desktop: FakeDesktop::new(Some("user copy")),
                llm: FakeLlm::new(reply),
                history: FakeHistory::default(),
                cancelled: AtomicBool::new(false),
                settings: CommandSettings::default(),
            }
        }
        fn run_with(
            &self,
            hold_error: Option<String>,
            transcript: Option<Result<&str, ProviderError>>,
        ) -> Outcome {
            let ports = Ports {
                desktop: &self.desktop,
                llm: &self.llm,
                history: &self.history,
            };
            let input = Run {
                settings: &self.settings,
                key_up: Instant::now(),
                cancelled: &self.cancelled,
                hold_error,
            };
            let mut record = DictationRecord::new(SystemTime::now(), "parakeet");
            record.kind = timing::COMMAND_KIND.into();
            run(&ports, input, record, |record| {
                transcript.map(|outcome| {
                    outcome.map(|text| {
                        record.text = text.into();
                        record.provider_returned = Some(timing::elapsed_ms(Instant::now()));
                    })
                })
            })
        }
        fn run(&self, instruction: &str) -> Outcome {
            self.run_with(None, Some(Ok(instruction)))
        }
        fn rows(&self) -> Vec<DictationRecord> {
            self.history.0.borrow().clone()
        }
        fn pasted(&self) -> Vec<String> {
            self.desktop.pasted.borrow().clone()
        }
    }

    #[test]
    fn the_reply_is_pasted_once_and_the_clipboard_restored() {
        let h = Harness::new(text("Ciao a tutti, buona giornata!"));
        assert_eq!(h.run("Scrivi un saluto breve"), Outcome::Pasted);
        assert_eq!(h.pasted(), ["Ciao a tutti, buona giornata!"]);
        assert_eq!(h.desktop.clipboard.borrow().as_deref(), Some("user copy"));
        assert_eq!(
            *h.llm.requests.borrow(),
            [Request {
                model: "gpt-6-luna".into(),
                reasoning_effort: "low".into(),
                instruction: "Scrivi un saluto breve".into(),
            }]
        );
        let [row] = &h.rows()[..] else {
            panic!("one row")
        };
        assert_eq!(row.kind, "command");
        assert_eq!(row.text, "Scrivi un saluto breve");
        assert_eq!(row.outcome, Some(CommandOutcome::Pasted));
        assert_eq!(row.llm_model.as_deref(), Some("gpt-6-luna"));
        assert_eq!((row.tokens_in, row.tokens_out), (Some(92), Some(18)));
        assert_eq!(row.reply.as_deref(), Some("Ciao a tutti, buona giornata!"));
        assert!(row.error.is_none());
        // Stage offsets from key-up, in order: transcribed, LLM reply, pasted.
        let (transcribed, replied, pasted) = (
            row.provider_returned.unwrap(),
            row.llm_returned.unwrap(),
            row.pasted.unwrap(),
        );
        assert!(transcribed <= replied && replied <= pasted);
    }

    #[test]
    fn a_blank_model_setting_asks_the_default_model() {
        let mut h = Harness::new(text("Ciao"));
        h.settings.model = "  ".into();
        assert_eq!(h.run("Scrivi"), Outcome::Pasted);
        assert_eq!(h.llm.requests.borrow()[0].model, "gpt-6-luna");
        assert_eq!(h.rows()[0].llm_model.as_deref(), Some("gpt-6-luna"));
    }

    #[test]
    fn an_empty_instruction_makes_no_llm_call_and_writes_no_row() {
        let h = Harness::new(text("never"));
        assert_eq!(h.run_with(None, None), Outcome::Silent);
        assert!(h.llm.requests.borrow().is_empty());
        assert!(h.rows().is_empty());
        assert!(h.pasted().is_empty());
        assert_eq!(h.desktop.clipboard.borrow().as_deref(), Some("user copy"));
    }

    #[test]
    fn a_timeout_or_an_api_error_fails_with_nothing_pasted() {
        for error in [
            ProviderError::Timeout,
            ProviderError::Http {
                status: 404,
                body: "model_not_found".into(),
            },
            ProviderError::Auth("OpenAI authentication rejected (HTTP 401)".into()),
            ProviderError::Network("offline".into()),
        ] {
            let h = Harness::new(Err(error.clone()));
            assert_eq!(
                h.run("Scrivi un saluto"),
                Outcome::Failed(Failure::Command(error.clone()))
            );
            assert!(h.pasted().is_empty(), "{error:?}");
            let [row] = &h.rows()[..] else {
                panic!("one row")
            };
            assert_eq!(row.outcome, Some(CommandOutcome::Failed));
            assert!(row.error.is_some());
            assert!(row.llm_returned.is_some());
            assert!(row.pasted.is_none());
        }
    }

    #[test]
    fn an_empty_reply_fails_but_keeps_its_tokens() {
        let h = Harness::new(text(" \n"));
        assert!(matches!(
            h.run("Scrivi un saluto"),
            Outcome::Failed(Failure::Command(_))
        ));
        assert!(h.pasted().is_empty());
        let row = &h.rows()[0];
        assert_eq!(row.outcome, Some(CommandOutcome::Failed));
        assert_eq!(row.tokens_out, Some(18));
    }

    #[test]
    fn esc_during_the_wait_discards_a_late_reply() {
        let mut h = Harness::new(text("too late"));
        h.llm.late = true;
        assert_eq!(h.run("Scrivi un saluto"), Outcome::Cancelled);
        assert!(h.pasted().is_empty());
        let [row] = &h.rows()[..] else {
            panic!("one row")
        };
        assert_eq!(row.outcome, Some(CommandOutcome::Cancelled));
        assert!(row.reply.is_none() && row.pasted.is_none() && row.error.is_none());
    }

    #[test]
    fn esc_before_the_request_makes_no_llm_call() {
        let h = Harness::new(text("never"));
        h.cancelled.store(true, Ordering::Release);
        assert_eq!(h.run("Scrivi un saluto"), Outcome::Cancelled);
        assert!(h.llm.requests.borrow().is_empty());
        assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Cancelled));
        // Esc aborts a live transcription, which then reports an error: still
        // a cancel, with no cross or notification.
        let h = Harness::new(text("never"));
        h.cancelled.store(true, Ordering::Release);
        assert_eq!(
            h.run_with(None, Some(Err(ProviderError::Timeout))),
            Outcome::Cancelled
        );
        assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Cancelled));
        assert!(h.rows()[0].error.is_none());
    }

    #[test]
    fn a_failed_transcription_or_a_capped_hold_makes_no_llm_call() {
        let h = Harness::new(text("never"));
        assert_eq!(
            h.run_with(None, Some(Err(ProviderError::Timeout))),
            Outcome::Failed(Failure::Transcription(ProviderError::Timeout))
        );
        let h2 = Harness::new(text("never"));
        assert!(matches!(
            h2.run_with(Some("hold exceeded 120 s".into()), Some(Ok("Scrivi"))),
            Outcome::Failed(Failure::Command(_))
        ));
        for h in [h, h2] {
            assert!(h.llm.requests.borrow().is_empty());
            assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Failed));
            assert!(h.rows()[0].error.is_some());
        }
    }

    #[test]
    fn a_failed_paste_fails_and_keeps_the_reply() {
        let mut h = Harness::new(text("Ciao"));
        h.desktop.fail_paste = true;
        assert!(matches!(
            h.run("Scrivi"),
            Outcome::Failed(Failure::Command(_))
        ));
        let row = &h.rows()[0];
        assert_eq!(row.reply.as_deref(), Some("Ciao"));
        assert!(row.pasted.is_none());
    }
}
