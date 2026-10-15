//! Lifetime usage counters, kept separately from `history.jsonl` so clearing the
//! history does not reset them. Words are counted by one deterministic rule:
//! whitespace-separated tokens of each successfully pasted transcript.

use crate::timing::DictationRecord;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tauri::{Emitter, Manager};

const FILE_NAME: &str = "stats.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub total_words: u64,
    pub dictations: u64,
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

pub struct StatsStore {
    path: PathBuf,
    inner: Mutex<Stats>,
}

impl StatsStore {
    fn open(path: PathBuf, history: &Path) -> Self {
        let stats = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
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
                Stats::default()
            }
        };
        Self {
            path,
            inner: Mutex::new(stats),
        }
    }

    pub fn get(&self) -> Stats {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add(&self, words: u64) -> Stats {
        let mut stats = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        stats.total_words = stats.total_words.saturating_add(words);
        stats.dictations = stats.dictations.saturating_add(1);
        if let Err(error) = save(&self.path, &stats) {
            eprintln!("Stats not saved: {error}");
        }
        *stats
    }
}

/// Counts every saved dictation in an existing history file.
fn backfill(history: &Path) -> Stats {
    let Ok(text) = std::fs::read_to_string(history) else {
        return Stats::default();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<DictationRecord>(line).ok())
        .filter_map(|record| counted_words(&record))
        .fold(Stats::default(), |mut stats, words| {
            stats.total_words += words;
            stats.dictations += 1;
            stats
        })
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
    app.manage(StatsStore::open(path, &history));
    Ok(())
}

/// Called for every finished dictation; counts it when it produced text.
pub fn record(app: &tauri::AppHandle, record: &DictationRecord) {
    let Some(words) = counted_words(record) else {
        return;
    };
    let Some(store) = app.try_state::<StatsStore>() else {
        return;
    };
    let stats = store.add(words);
    let _ = app.emit("stats://updated", stats);
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
        record
    }

    #[test]
    fn only_successful_dictations_with_text_count() {
        assert_eq!(counted_words(&record("dictation", "a b c", None)), Some(3));
        assert_eq!(counted_words(&record("dictation", "a b", Some("x"))), None);
        assert_eq!(counted_words(&record("dictation", "  ", None)), None);
        assert_eq!(counted_words(&record("benchmark", "a b c", None)), None);
    }

    #[test]
    fn first_run_backfills_from_history_then_persists_increments() {
        let dir = std::env::temp_dir().join(format!("parla-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let history = dir.join("history.jsonl");
        let lines = [
            record("dictation", "one two three", None),
            record("dictation", "failed words", Some("boom")),
            record("benchmark", "bench words here", None),
            record("dictation", "four five", None),
        ]
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&history, format!("{lines}\nnot json\n")).unwrap();

        let path = dir.join(FILE_NAME);
        let store = StatsStore::open(path.clone(), &history);
        assert_eq!(
            store.get(),
            Stats {
                total_words: 5,
                dictations: 2
            }
        );
        store.add(4);
        // A new process reads the file instead of recounting history.
        std::fs::write(&history, "").unwrap();
        let reopened = StatsStore::open(path, &history);
        assert_eq!(
            reopened.get(),
            Stats {
                total_words: 9,
                dictations: 3
            }
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
