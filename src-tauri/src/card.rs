//! The Card (PARLA-80): text Parla did not paste, shown above the pill with
//! buttons, until one is clicked or the next session starts. A dictation the
//! Safety Net caught opens here; PARLA-81 brings command replies with their
//! own tag, header and buttons.

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
}

/// A Card button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// The clipboard, for good.
    Copy,
    /// The normal paste transaction into whatever has focus now.
    Paste,
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

/// What the Card did goes to the log only: history stays append-only.
pub fn log(what: &str) {
    eprintln!("Parla card: {what}");
}

/// A click on the Card of `generation`: closes it, then copies or pastes its
/// text. A Card already closed ignores it. Paste mirrors the tray's Retry: the
/// normal transaction with the clipboard restore, never auto-send's Enter.
pub fn act(app: &tauri::AppHandle, generation: u64, action: Action) {
    use tauri::Manager;
    let Some(card) = crate::overlay::take_card(app, generation) else {
        return;
    };
    let result = match action {
        Action::Copy => crate::inserter::copy_text(&card.text).map(|()| "copied"),
        Action::Paste => {
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
            .map(|_| "pasted")
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
}
