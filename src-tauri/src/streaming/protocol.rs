//! Provider event reconciliation. Partial text is never a deliverable transcript.

use crate::provider::ProviderError;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Xai,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Caption {
    pub stable: String,
    pub provisional: String,
}

impl Caption {
    pub fn new(mut stable: String, mut provisional: String) -> Self {
        let pending_count = provisional.chars().count();
        if pending_count > 320 {
            provisional = tail(&provisional, 320);
            stable.clear();
        } else {
            stable = tail(&stable, 320 - pending_count);
        }
        Self {
            stable,
            provisional,
        }
    }
}

fn tail(text: &str, count: usize) -> String {
    let skip = text.chars().count().saturating_sub(count);
    text.chars().skip(skip).collect()
}

pub enum Event {
    Ready,
    Caption(Caption),
    Final(String),
    Ignore,
}

struct Segment {
    start: f64,
    end: f64,
    text: String,
}

pub struct Transcript {
    provider: Provider,
    seen: HashSet<String>,
    text: String,
    item: Option<String>,
    segments: Vec<Segment>,
    interim: Option<Segment>,
}

impl Transcript {
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            seen: HashSet::new(),
            text: String::new(),
            item: None,
            segments: Vec::new(),
            interim: None,
        }
    }

    pub fn apply(&mut self, event: &Value) -> Result<Event, ProviderError> {
        if let Some(id) = event["event_id"].as_str() {
            if !self.seen.insert(id.to_owned()) {
                return Ok(Event::Ignore);
            }
            if self.seen.len() > 4096 {
                return Err(invalid("Too many streaming events"));
            }
        }
        let kind = event["type"].as_str().unwrap_or("");
        if kind == "error" || kind.ends_with(".failed") {
            // Provider errors can contain request content or secrets. Do not log them.
            return Err(invalid("Live transcription was rejected by the provider"));
        }
        match self.provider {
            Provider::OpenAi => match kind {
                "session.updated" | "transcription_session.updated" => Ok(Event::Ready),
                "conversation.item.input_audio_transcription.delta" => {
                    self.check_item(event)?;
                    self.text.push_str(
                        event["delta"]
                            .as_str()
                            .ok_or_else(|| invalid("Invalid live text delta"))?,
                    );
                    self.check_length()?;
                    // Brightness is a visual recency cue, not a confidence claim.
                    let split = self
                        .text
                        .char_indices()
                        .rev()
                        .filter(|(_, c)| c.is_whitespace())
                        .nth(3)
                        .map_or(0, |(i, _)| i + 1);
                    Ok(Event::Caption(Caption::new(
                        self.text[..split].into(),
                        self.text[split..].into(),
                    )))
                }
                "conversation.item.input_audio_transcription.completed" => {
                    self.check_item(event)?;
                    final_text(event["transcript"].as_str().unwrap_or(""))
                }
                _ => Ok(Event::Ignore),
            },
            Provider::Xai => match kind {
                "transcript.created" => Ok(Event::Ready),
                "transcript.partial" => {
                    let text = event["text"]
                        .as_str()
                        .ok_or_else(|| invalid("Invalid live transcript"))?;
                    if text.len() > 64000 {
                        return Err(invalid("Live transcript is too large"));
                    }
                    let start = event["start"]
                        .as_f64()
                        .ok_or_else(|| invalid("Missing live segment start"))?;
                    let duration = event["duration"]
                        .as_f64()
                        .ok_or_else(|| invalid("Missing live segment duration"))?;
                    if start < 0.0 || duration < 0.0 || !start.is_finite() || !duration.is_finite()
                    {
                        return Err(invalid("Invalid live segment time"));
                    }
                    let segment = Segment {
                        start,
                        end: start + duration,
                        text: text.into(),
                    };
                    if event["is_final"].as_bool().unwrap_or(false) {
                        self.segments.retain(|old| !overlap(old, &segment));
                        self.segments.push(segment);
                        self.segments.sort_by(|a, b| a.start.total_cmp(&b.start));
                        self.interim = None;
                    } else {
                        self.interim = Some(segment);
                    }
                    let stable = self
                        .segments
                        .iter()
                        .filter(|s| {
                            self.interim
                                .as_ref()
                                .is_none_or(|pending| !overlap(s, pending))
                        })
                        .map(|s| s.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let provisional = self
                        .interim
                        .as_ref()
                        .map(|s| s.text.clone())
                        .unwrap_or_default();
                    if stable.len() + provisional.len() > 64000 {
                        return Err(invalid("Live transcript is too large"));
                    }
                    Ok(Event::Caption(Caption::new(
                        if stable.is_empty() {
                            stable
                        } else {
                            format!("{stable} ")
                        },
                        provisional,
                    )))
                }
                "transcript.done" => {
                    if let Some(text) = event["text"].as_str().filter(|s| !s.trim().is_empty()) {
                        return final_text(text);
                    }
                    if self
                        .interim
                        .as_ref()
                        .is_some_and(|s| !s.text.trim().is_empty())
                    {
                        return Err(invalid(
                            "Live transcript ended before its last words were finalized",
                        ));
                    }
                    final_text(
                        &self
                            .segments
                            .iter()
                            .map(|s| s.text.as_str())
                            .collect::<Vec<_>>()
                            .join(" "),
                    )
                }
                _ => Ok(Event::Ignore),
            },
        }
    }

    fn check_item(&mut self, event: &Value) -> Result<(), ProviderError> {
        if let Some(id) = event["item_id"].as_str() {
            if self.item.as_deref().is_some_and(|old| old != id) {
                return Err(invalid("Unexpected extra live transcription turn"));
            }
            self.item = Some(id.into());
        }
        Ok(())
    }
    fn check_length(&self) -> Result<(), ProviderError> {
        if self.text.len() > 64000 {
            Err(invalid("Live transcript is too large"))
        } else {
            Ok(())
        }
    }
}

fn overlap(a: &Segment, b: &Segment) -> bool {
    (a.start - b.start).abs() < 0.001 || (a.start < b.end - 0.001 && b.start < a.end - 0.001)
}
fn invalid(message: &str) -> ProviderError {
    ProviderError::Other(message.into())
}
fn final_text(text: &str) -> Result<Event, ProviderError> {
    if text.trim().is_empty() || text.len() > 64000 {
        Err(invalid("Live transcription returned no usable final text"))
    } else {
        Ok(Event::Final(text.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_final_replaces_deltas_and_duplicate_events_are_ignored() {
        let mut state = Transcript::new(Provider::OpenAi);
        let event = json!({"type":"conversation.item.input_audio_transcription.delta","event_id":"a","item_id":"turn","delta":"I like cats"});
        assert!(matches!(state.apply(&event).unwrap(), Event::Caption(_)));
        assert!(matches!(state.apply(&event).unwrap(), Event::Ignore));
        let final_event = json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"turn","transcript":"I like cats."});
        assert!(
            matches!(state.apply(&final_event).unwrap(), Event::Final(text) if text == "I like cats.")
        );
    }

    #[test]
    fn xai_revisions_and_stitched_utterances_do_not_duplicate_words() {
        let mut state = Transcript::new(Provider::Xai);
        for event in [
            json!({"type":"transcript.partial","start":0,"duration":3,"text":"I like cat","is_final":false}),
            json!({"type":"transcript.partial","start":0,"duration":3,"text":"I like cats.","is_final":true}),
            json!({"type":"transcript.partial","start":3,"duration":3,"text":"Anche io.","is_final":true}),
            json!({"type":"transcript.partial","start":0,"duration":6,"text":"I like cats. Anche io.","is_final":true,"speech_final":true}),
            json!({"type":"transcript.partial","start":6,"duration":2,"text":"Thank you.","is_final":true}),
        ] {
            state.apply(&event).unwrap();
        }
        assert!(
            matches!(state.apply(&json!({"type":"transcript.done","duration":8})).unwrap(), Event::Final(text) if text == "I like cats. Anche io. Thank you.")
        );
    }

    #[test]
    fn xai_never_delivers_an_unfinalized_tail_or_empty_result() {
        let mut state = Transcript::new(Provider::Xai);
        state.apply(&json!({"type":"transcript.partial","start":0,"duration":2,"text":"unfinished","is_final":false})).unwrap();
        assert!(state.apply(&json!({"type":"transcript.done"})).is_err());
        let mut empty = Transcript::new(Provider::OpenAi);
        assert!(empty.apply(&json!({"type":"conversation.item.input_audio_transcription.completed","transcript":""})).is_err());
    }

    #[test]
    fn preview_is_bounded_and_preserves_unicode() {
        let preview = Caption::new("ciao è ".repeat(100), "日本語 ✓".into());
        assert!(preview.stable.chars().count() + preview.provisional.chars().count() <= 320);
        assert_eq!(preview.provisional, "日本語 ✓");
    }
}
