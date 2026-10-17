//! Lifetime usage counters, kept separately from `history.jsonl` so clearing the
//! history does not reset them. Words are counted by one deterministic rule:
//! whitespace-separated tokens of each successfully pasted transcript. Audio
//! sent to each provider/model/mode drives the estimated cost (PARLA-19).

use crate::{
    pricing::{self, BATCH, STREAMING},
    timing::DictationRecord,
};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{mpsc, Mutex},
    thread::JoinHandle,
};
use tauri::{Emitter, Manager};

const FILE_NAME: &str = "stats.json";
/// Files below this version predate usage tracking and get it backfilled.
const VERSION: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub total_words: u64,
    pub dictations: u64,
    /// Audio of counted dictations (benchmarks excluded).
    pub audio_ms: u64,
    /// Wall clock (Unix ms) of the earliest counted or billed row.
    pub first_used_at: Option<u64>,
    /// Audio sent per provider, model and mode, benchmarks included.
    pub usage: Vec<Usage>,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub provider_id: String,
    pub model: String,
    pub mode: String,
    pub audio_ms: u64,
    pub requests: u64,
}

/// What the Home page shows; cost is computed from `usage` with list prices.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub total_words: u64,
    pub dictations: u64,
    pub audio_ms: u64,
    pub first_used_at: Option<u64>,
    pub cost_usd: f64,
    /// Billed audio whose model has no built-in price.
    pub unpriced_audio_ms: u64,
}

impl Stats {
    pub fn summary(&self) -> Summary {
        let mut cost_usd = 0.0;
        let mut unpriced_audio_ms = 0;
        for usage in &self.usage {
            match pricing::cost_usd(
                &usage.provider_id,
                &usage.model,
                &usage.mode,
                usage.audio_ms,
            ) {
                Some(cost) => cost_usd += cost,
                None => unpriced_audio_ms += usage.audio_ms,
            }
        }
        Summary {
            total_words: self.total_words,
            dictations: self.dictations,
            audio_ms: self.audio_ms,
            first_used_at: self.first_used_at,
            cost_usd,
            unpriced_audio_ms,
        }
    }

    /// Adds one finished row. Returns false when it changed nothing.
    fn apply(&mut self, record: &DictationRecord) -> bool {
        let words = counted_words(record);
        let billed = billed_usage(record);
        if words.is_none() && billed.is_empty() {
            return false;
        }
        if let Some(words) = words {
            self.total_words = self.total_words.saturating_add(words);
            self.dictations = self.dictations.saturating_add(1);
            self.audio_ms = self.audio_ms.saturating_add(record.audio_ms);
        }
        for (model, mode) in billed {
            self.bill(&record.provider_id, &model, mode, record.audio_ms);
        }
        self.first_used_at = Some(
            self.first_used_at
                .map_or(record.started_at, |first| first.min(record.started_at)),
        );
        true
    }

    fn bill(&mut self, provider_id: &str, model: &str, mode: &str, audio_ms: u64) {
        let index = match self.usage.iter().position(|usage| {
            usage.provider_id == provider_id && usage.model == model && usage.mode == mode
        }) {
            Some(index) => index,
            None => {
                self.usage.push(Usage {
                    provider_id: provider_id.into(),
                    model: model.into(),
                    mode: mode.into(),
                    audio_ms: 0,
                    requests: 0,
                });
                self.usage.len() - 1
            }
        };
        let usage = &mut self.usage[index];
        usage.audio_ms = usage.audio_ms.saturating_add(audio_ms);
        usage.requests = usage.requests.saturating_add(1);
    }
}

/// The one counting rule: whitespace-separated tokens.
pub fn count_words(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

/// Only dictations that produced pasted text count; failures and benchmark rows
/// do not.
fn counted_words(record: &DictationRecord) -> Option<u64> {
    (record.kind == "dictation" && record.error.is_none() && !record.text.trim().is_empty())
        .then(|| count_words(&record.text))
}

/// The (model, mode) pairs a row was billed for. A provider is assumed to bill
/// when it returned a transcript, even if pasting then failed; a live session
/// that fell back to batch is billed for both, with the whole clip each. A
/// command run's instruction is billed like a dictation, but never counted.
fn billed_usage(record: &DictationRecord) -> Vec<(String, &'static str)> {
    if !matches!(
        record.kind.as_str(),
        "dictation" | "benchmark" | crate::timing::COMMAND_KIND
    ) || record.text.trim().is_empty()
    {
        return Vec::new();
    }
    // Rows written before models were recorded used the provider default.
    let model = record
        .model
        .clone()
        .unwrap_or_else(|| default_model(&record.provider_id).into());
    match record.transcription_mode.as_deref() {
        Some("streaming") => vec![(model, STREAMING)],
        Some("batch_fallback") => {
            let stream_model =
                record
                    .stream_model
                    .clone()
                    .unwrap_or_else(|| match record.provider_id.as_str() {
                        "openai" => "gpt-live-transcribe".into(),
                        _ => model.clone(),
                    });
            vec![(model, BATCH), (stream_model, STREAMING)]
        }
        _ => vec![(model, BATCH)],
    }
}

fn default_model(provider_id: &str) -> &'static str {
    match provider_id {
        "openai" => crate::provider::openai::DEFAULT_MODEL,
        "xai" => crate::provider::xai::DEFAULT_MODEL,
        "parakeet" => "parakeet-tdt-0.6b-v3",
        _ => "",
    }
}

pub struct StatsStore {
    inner: Mutex<Stats>,
    writer: mpsc::Sender<Message>,
    worker: Option<JoinHandle<()>>,
}

enum Message {
    Save(Stats),
    Flush(mpsc::Sender<Result<(), String>>),
    Shutdown,
}

impl StatsStore {
    fn open(path: PathBuf, history: &Path) -> std::io::Result<Self> {
        let stats = match std::fs::read(&path) {
            Ok(bytes) => {
                let stats: Stats = serde_json::from_slice(&bytes).unwrap_or_default();
                if stats.version < VERSION {
                    // Counters from before usage tracking: keep their words,
                    // take audio and usage from the transcripts still saved.
                    let backfilled = backfill(history);
                    let stats = Stats {
                        total_words: stats.total_words,
                        dictations: stats.dictations,
                        ..backfilled
                    };
                    if let Err(error) = save(&path, &stats) {
                        eprintln!("Stats usage backfill not saved: {error}");
                    }
                    stats
                } else {
                    stats
                }
            }
            // First run with counters: start from the transcripts already saved.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let stats = backfill(history);
                if let Err(error) = save(&path, &stats) {
                    eprintln!("Stats backfill not saved: {error}");
                }
                stats
            }
            Err(error) => {
                eprintln!("Stats unreadable: {error}");
                Stats {
                    version: VERSION,
                    ..Stats::default()
                }
            }
        };
        Self::with_writer(stats, move |stats| save(&path, &stats))
    }

    fn with_writer(
        stats: Stats,
        mut persist: impl FnMut(Stats) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (writer, messages) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("parla-stats".into())
            .spawn(move || {
                let mut last_result = Ok(());
                for message in messages {
                    match message {
                        Message::Save(stats) => {
                            last_result = persist(stats);
                            if let Err(error) = &last_result {
                                eprintln!("Stats not saved: {error}");
                            }
                        }
                        Message::Flush(reply) => {
                            let _ = reply.send(last_result.clone());
                        }
                        Message::Shutdown => break,
                    }
                }
            })?;
        Ok(Self {
            inner: Mutex::new(stats),
            writer,
            worker: Some(worker),
        })
    }

    pub fn get(&self) -> Stats {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Counts a finished row; `None` when it changed nothing.
    fn add(&self, record: &DictationRecord) -> Option<Stats> {
        let mut stats = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !stats.apply(record) {
            return None;
        }
        // Keep enqueueing under the short memory lock so snapshots cannot be
        // written out of order. The writer never takes this lock or runs here.
        if self.writer.send(Message::Save(stats.clone())).is_err() {
            eprintln!("Stats writer is unavailable");
        }
        Some(stats.clone())
    }

    /// Waits for all earlier snapshots to reach disk. Only used at shutdown.
    pub fn flush(&self) -> Result<(), String> {
        let (reply, response) = mpsc::channel();
        self.writer
            .send(Message::Flush(reply))
            .map_err(|_| "Stats writer is unavailable")?;
        response
            .recv()
            .map_err(|_| "Stats writer did not respond")?
    }
}

impl Drop for StatsStore {
    fn drop(&mut self) {
        let _ = self.writer.send(Message::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Counts every saved dictation in an existing history file.
fn backfill(history: &Path) -> Stats {
    let Ok(text) = std::fs::read_to_string(history) else {
        return Stats::default();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<DictationRecord>(line).ok())
        .fold(
            Stats {
                version: VERSION,
                ..Stats::default()
            },
            |mut stats, record| {
                stats.apply(&record);
                stats
            },
        )
}

fn save(path: &Path, stats: &Stats) -> Result<(), String> {
    let parent = path.parent().ok_or("Stats path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("json.tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&temporary).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec_pretty(stats).map_err(|e| e.to_string())?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&temporary, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

pub fn setup(app: &tauri::App) -> Result<(), String> {
    let history = crate::history::path(app.handle())?;
    let path = history
        .parent()
        .ok_or("History path has no parent")?
        .join(FILE_NAME);
    app.manage(StatsStore::open(path, &history).map_err(|error| error.to_string())?);
    Ok(())
}

/// Called for every finished dictation and benchmark row.
pub fn record(app: &tauri::AppHandle, record: &DictationRecord) {
    let Some(store) = app.try_state::<StatsStore>() else {
        return;
    };
    if let Some(stats) = store.add(record) {
        let _ = app.emit("stats://updated", stats.summary());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn counts_whitespace_separated_words() {
        assert_eq!(count_words(""), 0);
        assert_eq!(count_words("   \n\t "), 0);
        assert_eq!(count_words("Ciao"), 1);
        assert_eq!(count_words("  Hello,   my name\nis Mattia. "), 5);
        assert_eq!(count_words("sono uno sviluppatore"), 3);
    }

    fn record(kind: &str, text: &str, error: Option<&str>) -> DictationRecord {
        let mut record = DictationRecord::new(SystemTime::now(), "openai");
        record.kind = kind.into();
        record.text = text.into();
        record.error = error.map(Into::into);
        record.audio_ms = 1_000;
        record
    }

    /// (total_words, dictations)
    fn counts(stats: &Stats) -> (u64, u64) {
        (stats.total_words, stats.dictations)
    }

    /// (audio_ms, requests) billed for one model and mode.
    fn usage(stats: &Stats, model: &str, mode: &str) -> Option<(u64, u64)> {
        stats
            .usage
            .iter()
            .find(|usage| usage.model == model && usage.mode == mode)
            .map(|usage| (usage.audio_ms, usage.requests))
    }

    #[test]
    fn only_successful_dictations_with_text_count() {
        assert_eq!(counted_words(&record("dictation", "a b c", None)), Some(3));
        assert_eq!(counted_words(&record("dictation", "a b", Some("x"))), None);
        assert_eq!(counted_words(&record("dictation", "  ", None)), None);
        assert_eq!(counted_words(&record("benchmark", "a b c", None)), None);
    }

    #[test]
    fn command_runs_count_toward_no_dictation_total_but_bill_their_speech() {
        let mut stats = Stats::default();
        let mut command = record(crate::timing::COMMAND_KIND, "make this formal", None);
        command.model = Some("gpt-4o-transcribe".into());
        assert!(stats.apply(&command));
        assert_eq!(counts(&stats), (0, 0));
        assert_eq!(stats.audio_ms, 0);
        assert_eq!(usage(&stats, "gpt-4o-transcribe", BATCH), Some((1_000, 1)));
    }

    #[test]
    fn usage_is_billed_per_model_and_mode() {
        let mut stats = Stats::default();
        let mut batch = record("dictation", "one two", None);
        batch.provider_id = "xai".into();
        batch.model = Some("grok-voice-transcribe-2.0".into());
        batch.transcription_mode = Some("batch".into());
        let mut streamed = batch.clone();
        streamed.transcription_mode = Some("streaming".into());
        streamed.audio_ms = 3_000;
        assert!(stats.apply(&batch));
        assert!(stats.apply(&streamed));
        assert!(stats.apply(&streamed));
        assert_eq!(counts(&stats), (6, 3));
        assert_eq!(stats.audio_ms, 7_000);
        let model = "grok-voice-transcribe-2.0";
        assert_eq!(usage(&stats, model, BATCH), Some((1_000, 1)));
        assert_eq!(usage(&stats, model, STREAMING), Some((6_000, 2)));
        assert_eq!(stats.first_used_at, Some(batch.started_at));
    }

    #[test]
    fn fallback_bills_both_the_live_session_and_the_batch_request() {
        let mut stats = Stats::default();
        let mut row = record("dictation", "ciao", None);
        row.model = Some("gpt-transcribe".into());
        row.transcription_mode = Some("batch_fallback".into());
        row.stream_model = Some("gpt-live-transcribe".into());
        stats.apply(&row);
        assert_eq!(usage(&stats, "gpt-transcribe", BATCH), Some((1_000, 1)));
        assert_eq!(
            usage(&stats, "gpt-live-transcribe", STREAMING),
            Some((1_000, 1))
        );
        assert_eq!(stats.audio_ms, 1_000, "audio time counts the clip once");
    }

    #[test]
    fn benchmarks_cost_but_do_not_count_and_failures_are_free() {
        let mut stats = Stats::default();
        let mut bench = record("benchmark", "bench words", None);
        bench.model = Some("gpt-transcribe".into());
        assert!(stats.apply(&bench));
        assert_eq!(counts(&stats), (0, 0));
        assert_eq!(stats.audio_ms, 0);
        assert_eq!(usage(&stats, "gpt-transcribe", BATCH), Some((1_000, 1)));

        // Paste failed after the provider returned text: billed, not counted.
        // Rows without a model (older history) use the provider default.
        assert!(stats.apply(&record("dictation", "some text", Some("Paste failed"))));
        assert_eq!(counts(&stats), (0, 0));
        let default_model = crate::provider::openai::DEFAULT_MODEL;
        assert_eq!(usage(&stats, default_model, BATCH), Some((2_000, 2)));

        let before = stats.clone();
        assert!(!stats.apply(&record("dictation", "", Some("offline"))));
        assert!(!stats.apply(&record("dictation", "  ", None)));
        assert_eq!(stats, before);
    }

    #[test]
    fn summary_prices_usage_and_reports_unpriced_audio() {
        let mut stats = Stats::default();
        let hour = 3_600_000;
        stats.bill("xai", "grok-voice-transcribe-2.0", STREAMING, hour);
        stats.bill("parakeet", "parakeet-tdt-0.6b-v3", BATCH, hour);
        stats.bill("openai", "future-model", BATCH, 60_000);
        let summary = stats.summary();
        assert!((summary.cost_usd - 0.20).abs() < 1e-9);
        assert_eq!(summary.unpriced_audio_ms, 60_000);
    }

    #[test]
    fn slow_disk_does_not_block_updates_and_flush_preserves_order() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (saved, snapshots) = mpsc::channel();
        let store = std::sync::Arc::new(
            StatsStore::with_writer(Stats::default(), move |stats| {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                saved.send(stats).unwrap();
                Ok(())
            })
            .unwrap(),
        );
        store.add(&record("dictation", "a b c", None));
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done, completed) = mpsc::channel();
        let updating = store.clone();
        let update = std::thread::spawn(move || {
            updating.add(&record("dictation", "d e f g", None));
            done.send(updating.get()).unwrap();
        });
        let observed = completed.recv_timeout(Duration::from_secs(2));
        // Unblock even if the assertion fails, so teardown cannot hang.
        release.send(()).unwrap();
        release.send(()).unwrap();
        update.join().unwrap();
        assert_eq!(counts(&observed.unwrap()), (7, 2));
        store.flush().unwrap();
        assert_eq!(counts(&snapshots.recv().unwrap()), (3, 1));
        assert_eq!(counts(&snapshots.recv().unwrap()), (7, 2));
    }

    #[test]
    fn dropping_store_drains_queued_updates_and_flush_reports_write_errors() {
        let (saved, snapshots) = std::sync::mpsc::channel();
        let store = StatsStore::with_writer(Stats::default(), move |stats| {
            saved.send(stats).unwrap();
            Ok(())
        })
        .unwrap();
        store.add(&record("dictation", "a b", None));
        store.add(&record("dictation", "c d e f g", None));
        assert_eq!(store.add(&record("dictation", "", Some("x"))), None);
        drop(store);
        assert_eq!(counts(&snapshots.into_iter().last().unwrap()), (7, 2));
        let failing =
            StatsStore::with_writer(Stats::default(), |_| Err("disk full".into())).unwrap();
        failing.add(&record("dictation", "a", None));
        assert_eq!(failing.flush(), Err("disk full".into()));
        assert_eq!(failing.get().total_words, 1);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("parla-stats-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_history(path: &Path, rows: &[DictationRecord]) {
        let lines = rows
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, format!("{lines}\nnot json\n")).unwrap();
    }

    #[test]
    fn first_run_backfills_from_history_then_persists_increments() {
        let dir = temp_dir("first-run");
        let history = dir.join("history.jsonl");
        write_history(
            &history,
            &[
                record("dictation", "one two three", None),
                record("dictation", "failed words", Some("boom")),
                record("benchmark", "bench words here", None),
                record("dictation", "four five", None),
            ],
        );

        let path = dir.join(FILE_NAME);
        let store = StatsStore::open(path.clone(), &history).unwrap();
        let stats = store.get();
        assert_eq!(counts(&stats), (5, 2));
        assert_eq!(stats.audio_ms, 2_000);
        // Dictations, the failed row with text and the benchmark were billed.
        let default_model = crate::provider::openai::DEFAULT_MODEL;
        assert_eq!(usage(&stats, default_model, BATCH), Some((4_000, 4)));
        store.add(&record("dictation", "six seven eight nine", None));
        store.flush().unwrap();
        // A new process reads the file instead of recounting history.
        std::fs::write(&history, "").unwrap();
        let reopened = StatsStore::open(path, &history).unwrap();
        assert_eq!(counts(&reopened.get()), (9, 3));
        assert_eq!(reopened.get().audio_ms, 3_000);
        drop(reopened);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn counters_from_before_usage_tracking_keep_words_and_gain_usage() {
        let dir = temp_dir("upgrade");
        let history = dir.join("history.jsonl");
        write_history(&history, &[record("dictation", "still saved", None)]);
        let path = dir.join(FILE_NAME);
        // Words survived an earlier history Clear, so they exceed the history.
        std::fs::write(&path, r#"{"total_words":120,"dictations":9}"#).unwrap();

        let store = StatsStore::open(path.clone(), &history).unwrap();
        let stats = store.get();
        assert_eq!(counts(&stats), (120, 9));
        assert_eq!(stats.audio_ms, 1_000);
        assert_eq!(stats.version, VERSION);
        assert!(stats.first_used_at.is_some());
        drop(store);
        // Upgraded once: the next start does not backfill again.
        write_history(&history, &[]);
        let reopened = StatsStore::open(path, &history).unwrap();
        assert_eq!(reopened.get(), stats);
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }
}
