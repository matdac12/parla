//! Runs one clip through every provider, one after another, without touching
//! the clipboard or the dictation failure path.
//!
//! Numbers (per provider, each measured from that provider's own turn):
//! - `request_ms`: just the `transcribe` call (encode + upload + decode for the
//!   cloud providers, inference for local ones).
//! - `total_ms`: from the moment the clip is handed to that provider (before its
//!   availability check) until its text is ready. A benchmark has no key-up, so
//!   "clip ready" plays that role; it is `request_ms` plus a few ms of bookkeeping.
//!
//! Providers run sequentially so they do not compete for bandwidth or CPU, and
//! there is no warm-up request: the first cloud request pays its TLS handshake,
//! exactly as the first dictation after idle does.
use crate::{
    clip::Clip,
    flow::DictationGuard,
    provider::{redact_secrets, Availability, SttProvider},
    settings::Settings,
    timing::DictationRecord,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BenchResult {
    pub provider_id: String,
    pub display_name: String,
    pub request_ms: Option<u64>,
    pub total_ms: Option<u64>,
    pub text: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BenchRun {
    pub id: String,
    pub started_at: u64,
    pub audio_ms: u64,
    pub results: Vec<BenchResult>,
}

/// `bench://progress` payload. `state` is `pending`, `running`, `done`,
/// `error` or `skipped`; the last three carry the finished `result`.
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub run_id: String,
    pub provider_id: String,
    pub display_name: String,
    pub state: String,
    pub result: Option<BenchResult>,
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

fn run_one(provider: &dyn SttProvider, clip: &Clip, settings: &Settings) -> BenchResult {
    let turn = Instant::now();
    let mut result = BenchResult {
        provider_id: provider.id().into(),
        display_name: provider.display_name().into(),
        request_ms: None,
        total_ms: None,
        text: None,
        error: None,
    };
    let availability = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        provider.availability(settings)
    }));
    match availability {
        Ok(Availability::Available) => {}
        Ok(Availability::Unavailable(reason)) => {
            result.error = Some(redact_secrets(&reason));
            return result;
        }
        Err(_) => {
            result.error = Some("Provider crashed".into());
            return result;
        }
    }
    let request = Instant::now();
    // One misbehaving provider must not abort the rest of the run.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        provider.transcribe(clip, settings)
    }));
    result.request_ms = Some(ms(request));
    result.total_ms = Some(ms(turn));
    match outcome {
        Ok(Ok(text)) if text.trim().is_empty() => {
            result.error = Some("Provider returned an empty transcript".into())
        }
        Ok(Ok(text)) => result.text = Some(text),
        Ok(Err(error)) => result.error = Some(redact_secrets(&error.to_string())),
        Err(_) => result.error = Some("Provider crashed".into()),
    }
    result
}

/// Local providers load their model on the first call, which would land in
/// `request_ms` and skew the comparison. Before timing them, transcribe half a
/// second of silence and throw the outcome away.
const LOCAL_PROVIDERS: [&str; 1] = ["parakeet"];

fn warm_up(provider: &dyn SttProvider, settings: &Settings) {
    if !LOCAL_PROVIDERS.contains(&provider.id())
        || !matches!(provider.availability(settings), Availability::Available)
    {
        return;
    }
    let silence = Clip::new(vec![0; crate::clip::SAMPLE_RATE as usize / 2]);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        provider.transcribe(&silence, settings)
    }));
}

fn history_row(
    run_id: &str,
    started_at: SystemTime,
    audio_ms: u64,
    result: &BenchResult,
) -> DictationRecord {
    let mut row = DictationRecord::new(started_at, &result.provider_id);
    row.kind = "benchmark".into();
    row.bench_run_id = Some(run_id.into());
    row.audio_ms = audio_ms;
    // Offsets are relative to the provider's turn: the request began
    // `total - request` ms in and its text arrived at `total`.
    row.audio_ready = result
        .total_ms
        .zip(result.request_ms)
        .map(|(t, r)| t.saturating_sub(r));
    row.provider_returned = result.total_ms;
    row.text = result.text.clone().unwrap_or_default();
    row.error = result.error.clone();
    row
}

/// Sequential runner. `providers` is injected so tests need no network.
pub fn run_with_providers(
    clip: &Clip,
    settings: &Settings,
    providers: Vec<Arc<dyn SttProvider>>,
    mut on_progress: impl FnMut(Progress),
    mut on_row: impl FnMut(DictationRecord),
) -> BenchRun {
    let started_at = SystemTime::now();
    let wall = started_at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let id = format!("bench-{}", DictationRecord::new(started_at, "").id);
    let audio_ms = clip.duration().as_millis() as u64;
    let progress =
        |provider: &dyn SttProvider, state: &str, result: Option<&BenchResult>| Progress {
            run_id: id.clone(),
            provider_id: provider.id().into(),
            display_name: provider.display_name().into(),
            state: state.into(),
            result: result.cloned(),
        };
    for provider in &providers {
        on_progress(progress(provider.as_ref(), "pending", None));
    }
    let mut results = Vec::with_capacity(providers.len());
    for provider in &providers {
        if LOCAL_PROVIDERS.contains(&provider.id()) {
            on_progress(progress(provider.as_ref(), "warming", None));
            warm_up(provider.as_ref(), settings);
        }
        on_progress(progress(provider.as_ref(), "running", None));
        let result = run_one(provider.as_ref(), clip, settings);
        let state = match (&result.error, result.request_ms) {
            (None, _) => "done",
            (Some(_), None) => "skipped",
            (Some(_), Some(_)) => "error",
        };
        on_row(history_row(&id, started_at, audio_ms, &result));
        on_progress(progress(provider.as_ref(), state, Some(&result)));
        results.push(result);
    }
    BenchRun {
        id,
        started_at: wall.as_millis() as u64,
        audio_ms,
        results,
    }
}

fn result_from_row(row: DictationRecord) -> BenchResult {
    let display_name = crate::provider::registry::get(&row.provider_id)
        .map(|provider| provider.display_name().to_string())
        .unwrap_or_else(|| row.provider_id.clone());
    BenchResult {
        request_ms: row
            .provider_returned
            .zip(row.audio_ready)
            .map(|(returned, ready)| returned.saturating_sub(ready)),
        total_ms: row.provider_returned,
        text: (!row.text.is_empty()).then_some(row.text),
        error: row.error,
        provider_id: row.provider_id,
        display_name,
    }
}

/// Rebuilds runs from history rows (newest first, as `History::load_recent`
/// returns them). Rows without a `bench_run_id` are ignored. At most `limit`
/// runs, newest first; results within a run keep their execution order.
pub fn group_runs(rows: Vec<DictationRecord>, limit: usize) -> Vec<BenchRun> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<DictationRecord>> = HashMap::new();
    for row in rows {
        let Some(id) = row.bench_run_id.clone().filter(|_| row.kind == "benchmark") else {
            continue;
        };
        if !groups.contains_key(&id) {
            if order.len() >= limit {
                continue;
            }
            order.push(id.clone());
        }
        groups.entry(id).or_default().push(row);
    }
    order
        .into_iter()
        .filter_map(|id| {
            let mut rows = groups.remove(&id)?;
            rows.reverse();
            let first = rows.first()?;
            let (started_at, audio_ms) = (first.started_at, first.audio_ms);
            Some(BenchRun {
                id,
                started_at,
                audio_ms,
                results: rows.into_iter().map(result_from_row).collect(),
            })
        })
        .collect()
}

#[derive(Default)]
struct Inner {
    recording: Option<(u64, DictationGuard)>,
    next_token: u64,
    clip: Option<Arc<Clip>>,
    window_generation: u64,
    window_open: bool,
}

/// Managed state: the in-flight recording (which holds the dictation guard so
/// hotkey dictation cannot paste meanwhile) and the last recorded clip.
#[derive(Default)]
pub struct BenchSession(Mutex<Inner>);

impl BenchSession {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn is_recording(&self) -> bool {
        self.lock().recording.is_some()
    }
    pub fn open_window(&self) {
        let mut inner = self.lock();
        inner.window_generation = inner.window_generation.wrapping_add(1);
        inner.window_open = true;
    }
    pub fn close_window(&self) {
        self.lock().window_open = false;
    }
    pub fn window_generation(&self) -> Option<u64> {
        let inner = self.lock();
        inner.window_open.then_some(inner.window_generation)
    }
    /// Returns a token identifying this recording (for the auto-stop timer).
    /// A close/reopen during device startup rejects the old request and returns
    /// its guard, so the caller can stop capture before releasing admission.
    pub fn begin_recording(
        &self,
        guard: DictationGuard,
        generation: u64,
    ) -> Result<u64, DictationGuard> {
        let mut inner = self.lock();
        if !inner.window_open || inner.window_generation != generation {
            return Err(guard);
        }
        inner.next_token += 1;
        let token = inner.next_token;
        inner.recording = Some((token, guard));
        Ok(token)
    }
    /// Takes the active recording's guard. With `Some(token)` only that
    /// recording qualifies, so a stale timer cannot stop a newer recording.
    pub fn take_recording(&self, token: Option<u64>) -> Option<DictationGuard> {
        let mut inner = self.lock();
        match &inner.recording {
            Some((current, _)) if token.is_none_or(|t| t == *current) => {
                inner.recording.take().map(|(_, guard)| guard)
            }
            _ => None,
        }
    }
    /// The clip "Run again" reuses.
    pub fn set_clip(&self, clip: Clip) {
        self.lock().clip = Some(Arc::new(clip));
    }
    pub fn clip(&self) -> Option<Arc<Clip>> {
        self.lock().clip.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        id: &'static str,
        available: bool,
        output: Result<String, ProviderError>,
        calls: Arc<AtomicUsize>,
    }
    impl SttProvider for Fake {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            self.id
        }
        fn availability(&self, _: &Settings) -> Availability {
            if self.available {
                Availability::Available
            } else {
                Availability::Unavailable("Missing model".into())
            }
        }
        fn transcribe(&self, _: &Clip, _: &Settings) -> Result<String, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(3));
            self.output.clone()
        }
    }
    #[test]
    fn sequential_results_skip_unavailable_and_continue_after_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers: Vec<Arc<dyn SttProvider>> = [
            ("missing", false, Ok("never".into())),
            ("broken", true, Err(ProviderError::Timeout)),
            ("empty", true, Ok(" ".into())),
            ("working", true, Ok("hello".into())),
        ]
        .into_iter()
        .map(|(id, available, output)| {
            Arc::new(Fake {
                id,
                available,
                output,
                calls: calls.clone(),
            }) as Arc<dyn SttProvider>
        })
        .collect();
        let mut states = Vec::new();
        let mut rows = Vec::new();
        let run = run_with_providers(
            &Clip::new(vec![1; 16000]),
            &Settings::default(),
            providers,
            |p| states.push((p.provider_id.clone(), p.state.clone())),
            |r| rows.push(r),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(run.audio_ms, 1000);
        assert_eq!(
            run.results
                .iter()
                .map(|r| r.provider_id.as_str())
                .collect::<Vec<_>>(),
            ["missing", "broken", "empty", "working"]
        );
        assert_eq!(run.results[0].error.as_deref(), Some("Missing model"));
        assert_eq!(run.results[0].request_ms, None);
        assert!(run.results[1].error.as_ref().unwrap().contains("timed out"));
        assert!(run.results[2].error.is_some());
        assert_eq!(run.results[3].text.as_deref(), Some("hello"));
        assert!(run.results[3].total_ms.unwrap() >= run.results[3].request_ms.unwrap());
        assert!(run.results[3].request_ms.unwrap() >= 3);
        assert_eq!(states.last().unwrap(), &("working".into(), "done".into()));
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.kind == "benchmark"
            && r.bench_run_id.as_deref() == Some(&run.id)
            && r.pasted.is_none()));
        rows.reverse();
        let restored = group_runs(rows, 10);
        assert_eq!(restored, vec![run]);
    }
    #[test]
    fn local_provider_is_warmed_untimed_and_panics_are_isolated() {
        struct Panicky(&'static str, Arc<AtomicUsize>);
        impl SttProvider for Panicky {
            fn id(&self) -> &'static str {
                self.0
            }
            fn display_name(&self) -> &'static str {
                self.0
            }
            fn availability(&self, _: &Settings) -> Availability {
                if self.0 == "boom" {
                    panic!("availability");
                }
                Availability::Available
            }
            fn transcribe(&self, _: &Clip, _: &Settings) -> Result<String, ProviderError> {
                self.1.fetch_add(1, Ordering::SeqCst);
                Ok("ok".into())
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let providers: Vec<Arc<dyn SttProvider>> = vec![
            Arc::new(Panicky("boom", calls.clone())),
            Arc::new(Panicky("parakeet", calls.clone())),
        ];
        let run = run_with_providers(
            &Clip::new(vec![1; 16000]),
            &Settings::default(),
            providers,
            |_| {},
            |_| {},
        );
        assert_eq!(run.results[0].error.as_deref(), Some("Provider crashed"));
        assert_eq!(run.results[1].text.as_deref(), Some("ok"));
        // One discarded warm-up plus the timed call; "boom" never transcribed.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn stale_timer_token_cannot_stop_a_newer_recording() {
        let state = crate::flow::DictationState::default();
        let session = BenchSession::default();
        session.open_window();
        let generation = session.window_generation().unwrap();
        let first = session
            .begin_recording(state.try_begin().unwrap(), generation)
            .ok()
            .unwrap();
        assert!(state.try_begin().is_none(), "recording holds the guard");
        assert!(session.take_recording(Some(first)).is_some());
        let second = session
            .begin_recording(state.try_begin().unwrap(), generation)
            .ok()
            .unwrap();
        assert!(session.take_recording(Some(first)).is_none());
        assert!(session.is_recording());
        assert!(session.take_recording(Some(second)).is_some());
        assert!(!session.is_recording());
        assert!(state.try_begin().is_some());
    }
    #[test]
    fn closing_or_reopening_during_startup_rejects_the_old_recording_request() {
        let session = BenchSession::default();
        let state = crate::flow::DictationState::default();
        session.open_window();
        let generation = session.window_generation().unwrap();
        let guard = state.try_begin().unwrap();
        session.close_window();
        let rejected = session.begin_recording(guard, generation).err().unwrap();
        assert!(!session.is_recording());
        assert!(
            state.try_begin().is_none(),
            "guard must stay held until capture stops"
        );
        drop(rejected);
        session.open_window();
        let guard = state.try_begin().unwrap();
        assert!(session.begin_recording(guard, generation).is_err());
        let current = session.window_generation().unwrap();
        let token = session
            .begin_recording(state.try_begin().unwrap(), current)
            .ok()
            .unwrap();
        assert!(session.take_recording(Some(token)).is_some());
        assert!(state.try_begin().is_some());
    }
    #[test]
    fn grouping_ignores_legacy_rows_and_limits_runs_not_provider_rows() {
        let make = |id: &str, start: u64, provider: &str| {
            let mut row = DictationRecord::new(SystemTime::now(), provider);
            row.kind = "benchmark".into();
            row.bench_run_id = Some(id.into());
            row.started_at = start;
            row.audio_ready = Some(7);
            row.provider_returned = Some(19);
            row.text = "speech".into();
            row
        };
        let rows = vec![
            make("new", 20, "xai"),
            DictationRecord::new(SystemTime::now(), "openai"),
            make("new", 20, "openai"),
            make("old", 10, "openai"),
        ];
        assert!(group_runs(rows.clone(), 0).is_empty());
        let runs = group_runs(rows, 1);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "new");
        assert_eq!(runs[0].results.len(), 2);
        assert!(runs[0]
            .results
            .iter()
            .all(|r| r.request_ms == Some(12) && r.total_ms == Some(19)));
    }
}
