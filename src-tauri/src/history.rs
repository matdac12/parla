//! Append-only JSON-lines history of dictations (PARLA-5).
//!
//! Every `DictationRecord` is one line in `history.jsonl` in the app data dir.
//! Writes run on a dedicated background thread fed by a channel, so the
//! dictation path never touches the disk (and never before the paste). All
//! file access is serialized on that thread: appends, clears and reads share
//! one owner, which keeps the file consistent without a lock on the hot path.
//!
//! Reads are bounded. `load_recent` walks the file backwards in chunks, parses
//! each chunk once and stops at the newest `limit` rows, keeping only a bounded
//! partial-line carry. Corrupt lines are skipped and oversized lines dropped,
//! so neither a large history nor a corrupt tail can grow memory without bound.
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
/// Largest line assembled from the tail. A longer line is corrupt or not ours;
/// it is dropped instead of growing the carry without bound.
const MAX_LINE: usize = 1024 * 1024;

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
    Append(Box<DictationRecord>),
    Load {
        limit: usize,
        kinds: &'static [&'static str],
        respond: Sender<Vec<DictationRecord>>,
    },
    Clear {
        respond: Sender<Result<(), String>>,
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
        let _ = self.sender.send(Message::Append(Box::new(record)));
    }

    /// Newest-first records, at most `limit` of them.
    pub fn load_recent(&self, limit: usize) -> Vec<DictationRecord> {
        self.load_recent_kinds(limit, &[])
    }

    /// Like `load_recent`, but only rows of `kinds` (all when empty) count
    /// toward `limit`, so a view still gets up to `limit` rows among others.
    pub fn load_recent_kinds(
        &self,
        limit: usize,
        kinds: &'static [&'static str],
    ) -> Vec<DictationRecord> {
        let (respond, response) = mpsc::channel();
        if self
            .sender
            .send(Message::Load {
                limit: limit.min(MAX_LIMIT),
                kinds,
                respond,
            })
            .is_err()
        {
            return Vec::new();
        }
        response.recv().unwrap_or_default()
    }

    /// Truncates the file. Returns once the writer thread has done it, with the
    /// writer's error if truncation failed, so the command can report it.
    pub fn clear(&self) -> Result<(), String> {
        let (respond, response) = mpsc::channel();
        if self.sender.send(Message::Clear { respond }).is_err() {
            return Err("History writer is unavailable".into());
        }
        response
            .recv()
            .unwrap_or_else(|_| Err("History writer did not respond".into()))
    }
}

fn writer_loop(path: PathBuf, receiver: Receiver<Message>) {
    let mut writer = Writer::new(path);
    while let Ok(message) = receiver.recv() {
        match message {
            Message::Append(record) => writer.append(&record),
            Message::Load {
                limit,
                kinds,
                respond,
            } => {
                let _ = respond.send(load_filtered(writer.path(), limit, kinds));
            }
            Message::Clear { respond } => {
                let _ = respond.send(writer.clear().map_err(|error| error.to_string()));
            }
        }
    }
}

/// Keeps one append handle open. `O_APPEND` means writes always land at the
/// end. Opening heals a partial trailing line left by an interrupted write, so
/// the next record is not glued onto it.
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
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            // A crash mid-write can leave a line without its newline; add one so
            // the record we are about to write starts on a fresh line.
            if matches!(ends_without_newline(&self.path), Ok(true)) {
                file.write_all(b"\n")?;
            }
            self.file = Some(file);
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
        // One write for the whole record: a short write cannot split the JSON
        // from its newline and glue it to the next record.
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        match self.file() {
            Ok(file) => {
                if let Err(error) = file.write_all(&bytes) {
                    eprintln!("history: append failed: {error}");
                    // Reopen (and heal) before the next append.
                    self.file = None;
                }
            }
            Err(error) => eprintln!("history: opening the history file failed: {error}"),
        }
    }

    fn clear(&mut self) -> std::io::Result<()> {
        // A Windows append handle lacks FILE_WRITE_DATA, so `set_len` fails on
        // it; truncate through a separate write handle instead. Release the
        // append handle first and let the next append reopen it.
        self.file = None;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        Ok(())
    }
}

/// True when `path` exists, is non-empty, and does not end in `\n` — the mark
/// of an interrupted append.
fn ends_without_newline(path: &Path) -> std::io::Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    Ok(byte[0] != b'\n')
}

/// Newest-first parse from the end of the file. Reads `READ_CHUNK` at a time
/// and processes each chunk exactly once (no re-parsing of the scanned suffix):
/// `carry` holds only the partial line at the left edge, capped at `MAX_LINE`.
/// Returns as soon as it has `limit` records.
#[cfg(test)]
fn load_recent_from(path: &Path, limit: usize) -> Vec<DictationRecord> {
    load_filtered(path, limit, &[])
}

/// `load_recent_from` keeping only rows whose `kind` is in `kinds` (all rows
/// when it is empty).
fn load_filtered(path: &Path, limit: usize, kinds: &[&str]) -> Vec<DictationRecord> {
    let keep = |record: &DictationRecord| kinds.is_empty() || kinds.contains(&record.kind.as_str());
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
    let mut records: Vec<DictationRecord> = Vec::new();
    // Left-to-right bytes of the line being assembled; bounded by MAX_LINE.
    let mut carry: Vec<u8> = Vec::new();
    // Set when the current line exceeded MAX_LINE: its remaining bytes are
    // discarded until the next newline.
    let mut oversized = false;
    let mut start = end;
    while start > 0 && records.len() < limit {
        let chunk_start = start.saturating_sub(READ_CHUNK);
        let mut chunk = vec![0u8; (start - chunk_start) as usize];
        if file.seek(SeekFrom::Start(chunk_start)).is_err() || file.read_exact(&mut chunk).is_err()
        {
            break;
        }
        let mut cursor = chunk.len();
        if oversized {
            // Discard the tail of the oversized line through its terminating
            // newline; if this chunk has none, it is all part of that line.
            match chunk.iter().rposition(|byte| *byte == b'\n') {
                Some(index) => {
                    oversized = false;
                    cursor = index;
                }
                None => {
                    start = chunk_start;
                    continue;
                }
            }
        }
        while cursor > 0 {
            match chunk[..cursor].iter().rposition(|byte| *byte == b'\n') {
                Some(index) => {
                    let mut line = Vec::with_capacity(cursor - index - 1 + carry.len());
                    line.extend_from_slice(&chunk[index + 1..cursor]);
                    line.extend_from_slice(&carry);
                    if let Ok(record) = serde_json::from_slice::<DictationRecord>(&line) {
                        if keep(&record) {
                            records.push(record);
                            if records.len() >= limit {
                                return records;
                            }
                        }
                    }
                    carry.clear();
                    cursor = index;
                }
                None => {
                    // The whole prefix belongs to a line that continues to the
                    // left; prepend it to the carry (or drop an oversized line).
                    if chunk[..cursor].len() + carry.len() > MAX_LINE {
                        carry.clear();
                        oversized = true;
                    } else {
                        let mut extended = Vec::with_capacity(cursor + carry.len());
                        extended.extend_from_slice(&chunk[..cursor]);
                        extended.extend_from_slice(&carry);
                        carry = extended;
                    }
                    cursor = 0;
                }
            }
        }
        start = chunk_start;
    }
    // Reaching the start of the file completes the first line (which may lack a
    // trailing newline). An oversized first line is dropped.
    if start == 0 && !oversized && !carry.is_empty() {
        if let Ok(record) = serde_json::from_slice::<DictationRecord>(&carry) {
            if keep(&record) {
                records.push(record);
            }
        }
    }
    records
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
    fn kind_filter_still_returns_limit_rows_past_other_kinds() {
        let path = temp_path("kind");
        let mut rows = vec![record(1, "d1"), record(2, "d2")];
        for n in 3..8 {
            let mut bench = record(n, "b");
            bench.kind = "benchmark".into();
            rows.push(bench);
        }
        append_all(&path, &rows);
        let dictations = load_filtered(&path, 2, &["dictation"]);
        assert_eq!(
            dictations
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
            ["d2", "d1"]
        );
        assert_eq!(load_filtered(&path, 50, &["benchmark"]).len(), 5);
        assert_eq!(load_recent_from(&path, 50).len(), 7);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn command_runs_round_trip_next_to_dictations_without_benchmarks() {
        use crate::timing::{CommandOutcome, COMMAND_KIND};
        let path = temp_path("commands");
        let mut command = record(2, "make this more formal");
        command.kind = COMMAND_KIND.into();
        command.outcome = Some(CommandOutcome::Pasted);
        command.llm_model = Some("gpt-6-luna".into());
        command.tokens_in = Some(92);
        command.tokens_out = Some(18);
        command.llm_returned = Some(1_400);
        command.reply = Some("Buongiorno a tutti.".into());
        let mut bench = record(3, "b");
        bench.kind = "benchmark".into();
        append_all(&path, &[record(1, "d1"), command, bench]);

        let rows = load_filtered(&path, 50, &["dictation", COMMAND_KIND]);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["make this more formal", "d1"]
        );
        let command = &rows[0];
        assert_eq!(command.kind, "command");
        assert_eq!(command.outcome, Some(CommandOutcome::Pasted));
        assert_eq!(command.llm_model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(
            (command.tokens_in, command.tokens_out),
            (Some(92), Some(18))
        );
        assert_eq!(command.llm_returned, Some(1_400));
        assert_eq!(command.reply.as_deref(), Some("Buongiorno a tutti."));
        // A dictation row writes none of the command fields.
        let line = std::fs::read_to_string(&path).unwrap();
        let dictation_line = line.lines().next().unwrap();
        for field in ["outcome", "llm_model", "tokens_in", "reply"] {
            assert!(!dictation_line.contains(field), "{field}");
        }
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
    fn clear_truncates_an_open_append_handle_and_later_appends_start_fresh() {
        let path = temp_path("clear");
        let mut writer = Writer::new(path.clone());
        // Open the append handle first: on Windows `set_len` on that handle
        // fails, so clear must truncate through its own write handle.
        writer.append(&record(1, "old"));
        writer.clear().unwrap();
        assert_eq!(path.metadata().unwrap().len(), 0);
        writer.append(&record(2, "new"));
        let records = load_recent_from(&path, 50);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "new");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn clear_on_a_missing_file_creates_an_empty_one() {
        let path = temp_path("clear-missing").with_extension("absent");
        let mut writer = Writer::new(path.clone());
        writer.clear().unwrap();
        assert_eq!(path.metadata().unwrap().len(), 0);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_partial_trailing_line_is_healed_before_the_next_append() {
        let path = temp_path("heal");
        let good = serde_json::to_string(&record(1, "good")).unwrap();
        // An interrupted write: a line with no terminating newline.
        std::fs::write(&path, format!("{good}\n{{\"partial\":")).unwrap();
        let mut writer = Writer::new(path.clone());
        writer.append(&record(2, "new"));
        let records = load_recent_from(&path, 50);
        assert_eq!(records.len(), 2, "the appended record must not be glued on");
        assert_eq!(records[0].text, "new");
        assert_eq!(records[1].text, "good");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn oversized_corrupt_lines_are_dropped_without_unbounded_memory() {
        let path = temp_path("oversized");
        let good = serde_json::to_string(&record(1, "good")).unwrap();
        // One corrupt line longer than MAX_LINE and several read chunks, so the
        // oversized path is exercised.
        let huge = "x".repeat(MAX_LINE + 4 * READ_CHUNK as usize);
        std::fs::write(&path, format!("{good}\n{huge}")).unwrap();
        let records = load_recent_from(&path, 50);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "good");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn clear_reports_a_writer_error_instead_of_succeeding_silently() {
        // A directory in place of the file makes truncation fail.
        let path = temp_path("clear-error");
        std::fs::create_dir_all(&path).unwrap();
        let mut writer = Writer::new(path.clone());
        assert!(writer.clear().is_err());
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
