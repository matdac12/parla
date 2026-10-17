//! The Card (PARLA-80): text Parla did not paste, shown above the pill with
//! buttons, until one is clicked or the next session starts. A dictation the
//! Safety Net caught opens here, and so does every command text reply
//! (PARLA-81), with its own tag, header and buttons.

/// What the Card shows. The overlay renders it as given.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Card {
    pub tag: Tag,
    /// Quoted in muted text beside the tag, if any.
    pub header: Option<String>,
    pub text: String,
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
    Dismiss,
}

impl Card {
    /// A dictation with nowhere to land: Copy is the likely action.
    pub fn caught_dictation(text: String) -> Self {
        Self {
            tag: Tag::NotPasted,
            header: None,
            text,
            actions: vec![Action::Copy, Action::Paste],
            primary: Action::Copy,
        }
    }
}

impl Card {
    /// A command's text reply (PARLA-81, ADR-0003): never pasted unasked.
    /// With nothing editable in focus only Copy is offered; otherwise
    /// Replace over the selection, or Paste at the cursor. An edit leads with
    /// that, an answer with Copy.
    pub fn command_reply(
        instruction: String,
        text: String,
        kind: crate::command_mode::responses::Kind,
        selected: bool,
        verdict: crate::focus_probe::Verdict,
    ) -> Self {
        use crate::{command_mode::responses::Kind, focus_probe::Verdict};
        let insert = if selected {
            Action::Replace
        } else {
            Action::Paste
        };
        let actions = match (verdict, kind) {
            (Verdict::NotEditable, _) => vec![Action::Copy],
            (_, Kind::Edit) => vec![insert, Action::Copy],
            (_, Kind::Answer) => vec![Action::Copy, insert],
        };
        Self {
            tag: Tag::Command,
            header: Some(instruction),
            text,
            primary: actions[0],
            actions,
        }
    }
}

/// What the Card did goes to the log only: history stays append-only.
pub fn log(what: &str) {
    eprintln!("Parla card: {what}");
}

/// A click on the Card of `generation`: closes it, then copies or pastes its
/// text. A Card already closed ignores it. Paste mirrors the tray's Retry: the
/// normal transaction with the clipboard restore, never auto-send's Enter.
/// Replace is the same paste, over the selection that is still live; there
/// is no focus guard.
pub fn act(app: &tauri::AppHandle, generation: u64, action: Action) {
    use tauri::Manager;
    let Some(card) = crate::overlay::take_card(app, generation) else {
        return;
    };
    let result = match action {
        Action::Copy => crate::inserter::copy_text(&card.text).map(|()| "copied"),
        Action::Paste | Action::Replace => {
            let settings = app
                .state::<crate::settings::SharedSettings>()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            crate::inserter::paste_text_with_options(
                &card.text,
                std::time::Duration::from_millis(settings.restore_delay_ms),
                settings.paste_compatibility_mode,
                false,
            )
            .map(|_| match action {
                Action::Replace => "replaced",
                _ => "pasted",
            })
        }
        Action::Dismiss => Ok("dismissed"),
    };
    match result {
        Ok(done) => log(done),
        Err(error) => log(&format!("{action:?} failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{command_mode::responses::Kind, focus_probe::Verdict};

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
        let card = Card::command_reply(
            "Rendilo più personale".into(),
            "Ciao Giulia, grazie davvero!".into(),
            Kind::Edit,
            true,
            Verdict::Unknown,
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

    #[test]
    fn a_command_cards_buttons_follow_its_kind_the_selection_and_the_probe() {
        use Action::{Copy, Paste, Replace};
        use Kind::{Answer, Edit};
        use Verdict::{Editable, NotEditable, Unknown};
        let cases = [
            // An edit's main button puts it in the document.
            (Edit, true, Unknown, vec![Replace, Copy], Replace),
            (Edit, true, Editable, vec![Replace, Copy], Replace),
            (Edit, false, Unknown, vec![Paste, Copy], Paste),
            // An answer's is Copy; Replace or Paste stays on offer.
            (Answer, true, Unknown, vec![Copy, Replace], Copy),
            (Answer, false, Editable, vec![Copy, Paste], Copy),
            // Nothing editable: Copy only, whatever the kind.
            (Edit, true, NotEditable, vec![Copy], Copy),
            (Answer, false, NotEditable, vec![Copy], Copy),
        ];
        for (kind, selected, verdict, actions, primary) in cases {
            let card = Card::command_reply("x".into(), "y".into(), kind, selected, verdict);
            let case = format!("{kind:?}, selected {selected}, {verdict:?}");
            assert_eq!(card.actions, actions, "{case}");
            assert_eq!(card.primary, primary, "{case}");
        }
    }
}
