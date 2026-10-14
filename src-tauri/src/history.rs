//! Append-only JSON-lines history of dictations (PARLA-5).
//!
//! Every `DictationRecord` is one line in `history.jsonl` in the app data dir.
//! Writes run on a dedicated background thread fed by a channel, so the
//! dictation path never touches the disk (and never before the paste). All
//! file access is serialized on that thread: appends, clears and reads share
//! one owner, which keeps the file consistent without a lock on the hot path.
//!
//! Reads are bounded. `load_recent` walks the file backwards in chunks and
//! stops once it has the newest `limit` rows, so a large history never has to
//! fit in memory. Corrupt lines are skipped, never fatal.
use crate::timing::DictationRecord;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
};
use tauri::Manager;

/// Newest rows returned when the caller does not pick a limit.
pub const DEFAULT_LIMIT: usize = 200;
/// Hard cap so a bad caller cannot make the reader walk the whole file.
pub const MAX_LIMIT: usize = 2_000;
const FILE_NAME: &str = "history.jsonl";
/// Bytes read per backwards step while hunting for the newest rows.
const READ_CHUNK: u64 = 64 * 1024;

/// `history.jsonl` lives in the app data dir: `%APPDATA%\com.matdac6.parla` on
/// Windows, `~/.local/share/com.matdac6.parla` on Linux.
pub fn path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join(FILE_NAME))
        .map_err(|error| error.to_string())
}

/// Starts the background writer and registers it as app state. When the path
/// cannot be resolved or the thread cannot start, history is disabled and the
/// stored `History` is absent (append/load become no-ops).
pub fn setup(app: &tauri::App) -> Result<(), String> {
    let path = path(app.handle())?;
    let history = History::start(path).map_err(|error| error.to_string())?;
    app.manage(history);
    Ok(())
}

enum Message {
    Append(DictationRecord),
    Load {
        limit: usize,
        respond: Sender<Vec<DictationRecord>>,
    },
    Clear {
        respond: Sender<()>,
    },
}

/// Handle to the background writer. `Sender` is `Sync`, so the handle is shared
/// directly through Tauri's managed state.
pub struct History {
    sender: Sender<Message>,
}

impl History {
    fn start(path: PathBuf) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Message>();
        std::thread::Builder::new()
            .name("parla-history".into())
            .spawn(move || writer_loop(path, receiver))?;
        Ok(Self { sender })
    }

    /// Fire-and-forget append; the caller never waits for disk IO.
    pub fn append(&self, record: DictationRecord) {
        let _ = self.sender.send(Message::Append(record));
    }

    /// Newest-first records, at most `limit` of them.
    pub fn load_recent(&self, limit: usize) -> Vec<DictationRecord> {
        let (respond, response) = mpsc::channel();
        if self
            .sender
            .send(Message::Load {
                limit: limit.min(MAX_LIMIT),
                respond,
            })
            .is_err()
        {
            return Vec::new();
        }
        response.recv().unwrap_or_default()
    }

    /// Truncates the file. Returns once the writer thread has done it.
    pub fn clear(&self) {
        let (respond, response) = mpsc::channel();
        if self.sender.send(Message::Clear { respond }).is_ok() {
            let _ = response.recv();
        }
    }
}

fn writer_loop(path: PathBuf, receiver: Receiver<Message>) {
    let mut writer = Writer::new(path);
    while let Ok(message) = receiver.recv() {
        match message {
            Message::Append(record) => writer.append(&record),
            Message::Load { limit, respond } => {
                let _ = respond.send(load_recent_from(writer.path(), limit));
            }
            Message::Clear { respond } => {
                writer.clear();
                let _ = respond.send(());
            }
        }
    }
}

/// Keeps one append handle open. `O_APPEND` means writes always land at the
/// end, including after `set_len(0)`.
struct Writer {
    path: PathBuf,
    file: Option<File>,
}

impl Writer {
    fn new(path: PathBuf) -> Self {
        Self { path, file: None }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn file(&mut self) -> std::io::Result<&mut File> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        Ok(self.file.as_mut().expect("file was just opened"))
    }

    fn append(&mut self, record: &DictationRecord) {
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(error) => {
                eprintln!("history: encoding a record failed: {error}");
                return;
            }
        };
        match self.file() {
            Ok(file) => {
                if let Err(error) = file
                    .write_all(line.as_bytes())
                    .and_then(|()| file.write_all(b"\n"))
                {
                    eprintln!("history: append failed: {error}");
                }
            }
            Err(error) => eprintln!("history: opening the history file failed: {error}"),
        }
    }

    fn clear(&mut self) {
        match self.file() {
            Ok(file) => {
                if let Err(error) = file.set_len(0) {
                    eprintln!("history: clear failed: {error}");
                }
            }
            Err(error) => eprintln!("history: opening the history file failed: {error}"),
        }
    }
}

/// Newest-first parse from the end of the file. Reads `READ_CHUNK` at a time
/// and grows the tail until it holds `limit` parseable lines (or reaches the
/// start of the file), so memory stays proportional to `limit`, not the file.
fn load_recent_from(path: &Path, limit: usize) -> Vec<DictationRecord> {
    if limit == 0 {
        return Vec::new();
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let end = match file.seek(SeekFrom::End(0)) {
        Ok(end) => end,
        Err(_) => return Vec::new(),
    };
    let mut tail: Vec<u8> = Vec::new();
    let mut start = end;
    loop {
        let head_partial = start > 0;
        let records = parse_tail(&tail, head_partial);
        if records.len() >= limit || !head_partial {
            return records.into_iter().take(limit).collect();
        }
        let chunk_start = start.saturating_sub(READ_CHUNK);
        let mut chunk = vec![0u8; (start - chunk_start) as usize];
        if file.seek(SeekFrom::Start(chunk_start)).is_err() || file.read_exact(&mut chunk).is_err()
        {
            return records.into_iter().take(limit).collect();
        }
        chunk.extend_from_slice(&tail);
        tail = chunk;
        start = chunk_start;
    }
}

/// Splits a tail buffer on newlines and returns newest-first records. Lines
/// that fail to parse (or are empty) are skipped. `head_partial` drops the
/// first line, which may be cut off when the buffer started mid-file.
fn parse_tail(tail: &[u8], head_partial: bool) -> Vec<DictationRecord> {
    let mut lines: Vec<&[u8]> = tail.split(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if head_partial && !lines.is_empty() {
        lines.remove(0);
    }
    lines
        .iter()
        .rev()
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::DictationRecord;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "parla-history-test-{}-{}-{}",
            std::process::id(),
            tag,
            unique
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE_NAME)
    }

    fn record(started_at: u64, text: &str) -> DictationRecord {
        let mut record = DictationRecord::new(
            UNIX_EPOCH + std::time::Duration::from_millis(started_at),
            "openai",
        );
        record.started_at = started_at;
        record.audio_ready = Some(10);
        record.provider_returned = Some(30);
        record.pasted = Some(50);
        record.audio_ms = 1_000;
        record.text = text.into();
        record
    }

    fn append_all(path: &Path, records: &[DictationRecord]) {
        let mut writer = Writer::new(path.to_path_buf());
        for record in records {
            writer.append(record);
        }
    }

    #[test]
    fn record_serialization_round_trips() {
        let original = record(1_234, "hello world");
        let line = serde_json::to_string(&original).unwrap();
        assert!(!line.contains('\n'), "a record must fit on one JSONL line");
        let decoded: DictationRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded.id, original.id);
        assert_eq!(decoded.started_at, 1_234);
        assert_eq!(decoded.provider_id, "openai");
        assert_eq!(decoded.pasted, Some(50));
        assert_eq!(decoded.text, "hello world");
        assert_eq!(decoded.kind, "dictation");
    }

    #[test]
    fn load_recent_returns_newest_first_and_respects_the_limit() {
        let path = temp_path("order");
        append_all(
            &path,
            &[record(1, "one"), record(2, "two"), record(3, "three")],
        );
        let all = load_recent_from(&path, 200);
        assert_eq!(
            all.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["three", "two", "one"]
        );
        let newest_two = load_recent_from(&path, 2);
        assert_eq!(newest_two.len(), 2);
        assert_eq!(newest_two[0].text, "three");
        assert_eq!(newest_two[1].text, "two");
        assert!(load_recent_from(&path, 0).is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn load_recent_spans_many_chunks_without_reading_everything() {
        let path = temp_path("chunks");
        let records: Vec<_> = (0..500).map(|n| record(n, &"x".repeat(300))).collect();
        append_all(&path, &records);
        assert!(
            path.metadata().unwrap().len() > READ_CHUNK,
            "fixture must exceed one read chunk to exercise the backwards scan"
        );
        let newest = load_recent_from(&path, 5);
        assert_eq!(newest.len(), 5);
        assert_eq!(newest[0].started_at, 499);
        assert_eq!(newest[4].started_at, 495);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn corrupt_and_blank_lines_are_skipped() {
        let path = temp_path("corrupt");
        let good = serde_json::to_string(&record(7, "kept")).unwrap();
        std::fs::write(&path, format!("not json\n\n{good}\n{{\"half\": \n{good}\n")).unwrap();
        let records = load_recent_from(&path, 50);
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.text == "kept"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn clear_truncates_and_later_appends_start_fresh() {
        let path = temp_path("clear");
        append_all(&path, &[record(1, "old")]);
        let mut writer = Writer::new(path.clone());
        writer.clear();
        assert_eq!(path.metadata().unwrap().len(), 0);
        writer.append(&record(2, "new"));
        let records = load_recent_from(&path, 50);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "new");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_file_loads_as_empty_instead_of_failing() {
        let path = temp_path("missing").with_extension("absent");
        assert!(load_recent_from(&path, 10).is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_record_created_now_has_no_local_clock_skew() {
        let record = DictationRecord::new(SystemTime::now(), "openai");
        assert!(record.started_at > 0);
        assert_eq!(record.key_up, 0);
    }
}
