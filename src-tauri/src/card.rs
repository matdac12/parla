//! The Card (PARLA-80): text Parla did not paste, shown above the pill with
//! buttons, until one is clicked or the next session starts. A dictation the
//! Safety Net caught opens here, and so does every command text reply
//! (PARLA-81), with its own tag, header and buttons. A reminder command
//! confirms the To-do it created here (PARLA-88), with Open and Undo: the one
//! Card that closes itself.

use crate::command_mode::{traccia, UrlOpener};

/// What the Card shows. The overlay renders it as given.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Card {
    pub tag: Tag,
    /// Quoted in muted text beside the tag, if any.
    pub header: Option<String>,
    pub text: String,
    /// A muted second line under the text: a reminder's day and time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The To-do a reminder Card's Open and Undo act on. Rust's only: the
    /// page never gets it.
    #[serde(skip)]
    pub todo: Option<Todo>,
    /// The footer's buttons, left to right. Dismiss is always the ✕.
    pub actions: Vec<Action>,
    /// The filled one.
    pub primary: Action,
}

/// The header's tag: its dot and label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tag {
    /// A neutral dot and "Not pasted · no text field".
    NotPasted,
    /// An orange dot and "Command".
    Command,
    /// An orange dot and "Reminder", with the countdown ring.
    Reminder,
}

/// The To-do Traccia created, as it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub id: String,
    /// Its panel in the dashboard.
    pub url: String,
}

/// A Card button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// The clipboard, for good.
    Copy,
    /// The normal paste transaction into whatever has focus now.
    Paste,
    /// Paste, over the selection the command read: it is still selected,
    /// since the Card never takes focus.
    Replace,
    /// A reminder's To-do in the dashboard, in the default browser.
    Open,
    /// Deletes the reminder's To-do (Traccia keeps it in its Trash).
    Undo,
    Dismiss,
}

impl Card {
    /// A dictation with nowhere to land: Copy is the likely action.
    pub fn caught_dictation(text: String) -> Self {
        Self {
            tag: Tag::NotPasted,
            header: None,
            text,
            detail: None,
            todo: None,
            actions: vec![Action::Copy, Action::Paste],
            primary: Action::Copy,
        }
    }

    /// A command's text reply (PARLA-81): the instruction in the header, and
    /// `actions` with the first one filled.
    pub fn command(instruction: String, text: String, actions: Vec<Action>) -> Self {
        Self {
            tag: Tag::Command,
            header: Some(instruction),
            text,
            detail: None,
            todo: None,
            primary: actions[0],
            actions,
        }
    }

    /// A reminder created (PARLA-88): its title as Traccia stored it, the
    /// day and time on the second line, Open filled, then Undo. No Copy:
    /// there is nothing to paste, and the text stays selectable.
    pub fn reminder(
        instruction: String,
        title: String,
        when_line: String,
        id: String,
        url: String,
    ) -> Self {
        Self {
            tag: Tag::Reminder,
            header: Some(instruction),
            text: title,
            detail: Some(when_line),
            todo: Some(Todo { id, url }),
            actions: vec![Action::Open, Action::Undo],
            primary: Action::Open,
        }
    }

    /// The reminder's confirmation closes after a few seconds; every other
    /// Card waits for a click or the next session.
    pub fn closes_itself(&self) -> bool {
        self.tag == Tag::Reminder
    }
}

/// What the Card did goes to the log only: history stays append-only.
pub fn log(what: &str) {
    eprintln!("Parla card: {what}");
}

/// A click on the Card of `generation`: closes it, then copies, pastes or
/// opens. A Card already closed ignores it. Paste mirrors the tray's Retry:
/// the normal transaction with the clipboard restore, never auto-send's
/// Enter. Replace is the same paste, over the selection that is still live;
/// there is no focus guard. Undo closes the Card only once Traccia has
/// deleted the To-do. Blocking: the caller runs it off the UI thread.
pub fn act(app: &tauri::AppHandle, generation: u64, action: Action) {
    use tauri::Manager;
    let settings = || {
        app.state::<crate::settings::SharedSettings>()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    };
    // Every action but Undo closes the Card first.
    let take = || crate::overlay::take_card(app, generation);
    let outcome = |result: Result<&str, String>| match result {
        Ok(done) => done.to_string(),
        Err(error) => format!("{action:?} failed: {error}"),
    };
    let line = match action {
        Action::Undo => {
            let settings = settings();
            let traccia = traccia::Remote(&settings);
            undo(
                &OnScreen { app, generation },
                |id| traccia.undo(id),
                |message| {
                    crate::failure::notify_command_error(
                        app,
                        &crate::provider::ProviderError::Other(message.into()),
                    )
                },
            )
        }
        Action::Copy => {
            take().map(|card| outcome(crate::inserter::copy_text(&card.text).map(|()| "copied")))
        }
        Action::Paste | Action::Replace => take().map(|card| {
            let done = if action == Action::Replace {
                "replaced"
            } else {
                "pasted"
            };
            let settings = settings();
            outcome(
                crate::inserter::paste_text_with_options(
                    &card.text,
                    std::time::Duration::from_millis(settings.restore_delay_ms),
                    settings.paste_compatibility_mode,
                    false,
                )
                .map(|_| done),
            )
        }),
        Action::Open => take().map(|card| {
            let base_url = traccia::TracciaConfig::resolved(&settings())
                .ok()
                .map(|config| config.base_url);
            let browser = crate::command_mode::tools::DefaultBrowser;
            open(&card, base_url.as_deref(), &browser)
                .map(String::from)
                .unwrap_or_else(|message| {
                    crate::failure::notify_command_error(
                        app,
                        &crate::provider::ProviderError::Other(message.clone()),
                    );
                    message
                })
        }),
        Action::Dismiss => take().map(|_| "dismissed".into()),
    };
    if let Some(line) = line {
        log(&line);
    }
}

/// Open: the To-do's panel, only if it is on the configured Traccia
/// (`base_url`). The log line, or why nothing opened (for the notification
/// too); neither names the url.
fn open(
    card: &Card,
    base_url: Option<&str>,
    browser: &dyn UrlOpener,
) -> Result<&'static str, String> {
    let Some(url) = card
        .todo
        .as_ref()
        .map(|todo| todo.url.as_str())
        .filter(|url| base_url.is_some_and(|base| traccia::on_base_url(url, base)))
    else {
        return Err("Open refused: the To-do is not on the Traccia URL".into());
    };
    browser
        .open(url)
        .map(|()| "opened the To-do")
        .map_err(|error| format!("Open failed: {error}"))
}

/// The reminder Card on screen, as Undo sees it.
trait Shown {
    /// Its To-do, the Card left open with its countdown stopped. None once
    /// it is gone, or while an Undo is already running.
    fn hold(&self) -> Option<Todo>;
    fn close(&self);
    /// Gives it its buttons back after a failed Undo.
    fn keep(&self);
}

/// The overlay's Card of one session.
struct OnScreen<'a> {
    app: &'a tauri::AppHandle,
    generation: u64,
}

impl Shown for OnScreen<'_> {
    fn hold(&self) -> Option<Todo> {
        crate::overlay::hold_card(self.generation)
    }
    fn close(&self) {
        let _ = crate::overlay::take_card(self.app, self.generation);
    }
    fn keep(&self) {
        crate::overlay::keep_card(self.app, self.generation);
    }
}

/// Undo: `delete` the To-do (a 5 s `DELETE`) with the Card still up. Done, the
/// Card closes; failed, `notify` says why and the Card stays, so Undo or Open
/// can be tried again. History is not touched: its row stays `created`.
/// Returns the log line; None when there was nothing to undo.
fn undo(
    shown: &dyn Shown,
    delete: impl FnOnce(&str) -> Result<(), traccia::Error>,
    notify: impl FnOnce(&str),
) -> Option<String> {
    let todo = shown.hold()?;
    Some(match delete(&todo.id) {
        Ok(()) => {
            shown.close();
            "reminder undone".into()
        }
        Err(error) => {
            let message = format!("Undo failed: {error}");
            notify(&message);
            shown.keep();
            message
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn a_caught_dictation_offers_copy_first_then_paste() {
        let card = Card::caught_dictation("Ricordami la fattura".into());
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "tag": "not_pasted",
                "header": null,
                "text": "Ricordami la fattura",
                "actions": ["copy", "paste"],
                "primary": "copy",
            })
        );
    }

    #[test]
    fn a_command_reply_is_tagged_and_headed_by_its_instruction() {
        let card = Card::command(
            "Rendilo più personale".into(),
            "Ciao Giulia, grazie davvero!".into(),
            vec![Action::Replace, Action::Copy],
        );
        assert_eq!(
            serde_json::to_value(&card).unwrap(),
            serde_json::json!({
                "tag": "command",
                "header": "Rendilo più personale",
                "text": "Ciao Giulia, grazie davvero!",
                "actions": ["replace", "copy"],
                "primary": "replace",
            })
        );
    }

    fn reminder() -> Card {
        Card::reminder(
            "ricordami di chiamare Luca domani alle 15".into(),
            "Chiamare Luca @CRM".into(),
            "Sun 18/10 · 15:00".into(),
            "01K8TODO0000000000000000AB".into(),
            format!("{BASE}/personal?todo=01K8TODO0000000000000000AB"),
        )
    }
    const BASE: &str = "https://traccia.example.ts.net";

    #[test]
    fn a_reminder_card_offers_open_filled_then_undo_and_no_copy() {
        let card = reminder();
        // The page gets no id or url: only Rust acts on them.
        assert_eq!(
            serde_json::to_value(&card).unwrap(),
            serde_json::json!({
                "tag": "reminder",
                "header": "ricordami di chiamare Luca domani alle 15",
                "text": "Chiamare Luca @CRM",
                "detail": "Sun 18/10 · 15:00",
                "actions": ["open", "undo"],
                "primary": "open",
            })
        );
        assert_eq!(
            card.todo,
            Some(Todo {
                id: "01K8TODO0000000000000000AB".into(),
                url: format!("{BASE}/personal?todo=01K8TODO0000000000000000AB"),
            })
        );
        // The one Card with a timer.
        assert!(card.closes_itself());
        assert!(!Card::caught_dictation("x".into()).closes_itself());
        assert!(!Card::command("x".into(), "y".into(), vec![Action::Copy]).closes_itself());
    }

    #[derive(Default)]
    struct Browser {
        opened: RefCell<Vec<String>>,
        fails: bool,
    }
    impl UrlOpener for Browser {
        fn open(&self, url: &str) -> Result<(), String> {
            self.opened.borrow_mut().push(url.into());
            if self.fails {
                Err("no browser".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn open_goes_only_to_the_configured_traccia() {
        let browser = Browser::default();
        assert_eq!(
            open(&reminder(), Some(BASE), &browser),
            Ok("opened the To-do")
        );
        assert_eq!(
            *browser.opened.borrow(),
            [format!("{BASE}/personal?todo=01K8TODO0000000000000000AB")]
        );
        // Another host, no Traccia configured, or no To-do: nothing opens,
        // and the log never names the url.
        let mut elsewhere = reminder();
        elsewhere.todo.as_mut().unwrap().url = "https://evil.example.com/personal".into();
        let browser = Browser::default();
        for (card, base) in [
            (&elsewhere, Some(BASE)),
            (&reminder(), None),
            (&Card::caught_dictation("x".into()), Some(BASE)),
        ] {
            assert_eq!(
                open(card, base, &browser),
                Err("Open refused: the To-do is not on the Traccia URL".to_string())
            );
        }
        assert!(browser.opened.borrow().is_empty());
        let failing = Browser {
            fails: true,
            ..Browser::default()
        };
        assert_eq!(
            open(&reminder(), Some(BASE), &failing),
            Err("Open failed: no browser".to_string())
        );
    }

    /// The overlay's side of a reminder Card.
    struct Screen {
        card: RefCell<Option<Card>>,
        /// An Undo holds it: no countdown, no second Undo.
        held: Cell<bool>,
        kept: Cell<u32>,
    }
    impl Screen {
        fn showing(card: Card) -> Self {
            Self {
                card: RefCell::new(Some(card)),
                held: Cell::new(false),
                kept: Cell::new(0),
            }
        }
    }
    impl Shown for Screen {
        fn hold(&self) -> Option<Todo> {
            if self.held.replace(true) {
                return None;
            }
            self.card.borrow().as_ref()?.todo.clone()
        }
        fn close(&self) {
            *self.card.borrow_mut() = None;
        }
        fn keep(&self) {
            self.held.set(false);
            self.kept.set(self.kept.get() + 1);
        }
    }

    #[test]
    fn undo_deletes_the_to_do_then_closes_the_card() {
        let screen = Screen::showing(reminder());
        let deleted = RefCell::new(Vec::new());
        let notified = RefCell::new(Vec::<String>::new());
        let line = undo(
            &screen,
            |id| {
                // Still on screen while the DELETE runs.
                assert!(screen.card.borrow().is_some());
                deleted.borrow_mut().push(id.to_string());
                Ok(())
            },
            |message| notified.borrow_mut().push(message.into()),
        );
        assert_eq!(line.as_deref(), Some("reminder undone"));
        assert_eq!(*deleted.borrow(), ["01K8TODO0000000000000000AB"]);
        assert!(screen.card.borrow().is_none());
        assert!(notified.borrow().is_empty());
    }

    #[test]
    fn a_failed_undo_notifies_and_the_card_stays_for_a_retry() {
        let screen = Screen::showing(reminder());
        let notified = RefCell::new(Vec::<String>::new());
        let line = undo(
            &screen,
            |_| Err(traccia::Error::Unreachable),
            |message| notified.borrow_mut().push(message.into()),
        );
        assert_eq!(
            line.as_deref(),
            Some("Undo failed: Traccia can't be reached. Is Tailscale on?")
        );
        assert_eq!(
            *notified.borrow(),
            ["Undo failed: Traccia can't be reached. Is Tailscale on?"]
        );
        assert_eq!(screen.card.borrow().as_ref(), Some(&reminder()));
        assert_eq!(screen.kept.get(), 1);
        // Undo again: it works this time.
        let line = undo(&screen, |_| Ok(()), |_| unreachable!());
        assert_eq!(line.as_deref(), Some("reminder undone"));
    }

    #[test]
    fn undo_on_a_card_gone_or_already_undoing_sends_nothing() {
        let screen = Screen::showing(reminder());
        screen.held.set(true);
        let never = |_: &str| -> Result<(), traccia::Error> { unreachable!() };
        assert_eq!(undo(&screen, never, |_| unreachable!()), None);
        let gone = Screen {
            card: RefCell::new(None),
            ..Screen::showing(reminder())
        };
        assert_eq!(undo(&gone, never, |_| unreachable!()), None);
    }
}
