//! An error-only log file (PARLA-102): `parla.log` in the app data dir, one
//! timestamped line per failure, so an autostart session (no console) and a
//! user's bug report have something to read.
//!
//! It is for failures only. A successful dictation never reaches it: nothing
//! in the recorder's resume timing or the timing summaries is logged, and a
//! write is a synchronous append, acceptable because failures are rare. On the
//! dictation path it is called only after the outcome is final (the failure
//! notification), never between key-down and paste. At about 1 MB the file is
//! moved to `parla.log.1`, replacing the previous one, so the two together
//! stay under about 2 MB.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const FILE_NAME: &str = "parla.log";
/// The size at which `parla.log` becomes `parla.log.1`.
const MAX_BYTES: u64 = 1024 * 1024;
/// A longer message is cut, so one runaway error cannot fill the file.
const MAX_MESSAGE: usize = 2_000;

static PATH: OnceLock<PathBuf> = OnceLock::new();
/// Serializes writes and remembers the last message, so a retry loop that
/// fails the same way every second leaves one line a minute, not 3,600 an hour.
static LAST: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// How long an identical message is held back.
const REPEAT_WINDOW: Duration = Duration::from_secs(60);

/// Prints to stderr like `eprintln!` and appends the same text to the log.
macro_rules! log_error {
    ($($arg:tt)*) => {{
        let message = format!($($arg)*);
        eprintln!("{message}");
        $crate::log::error(&message);
    }};
}
pub(crate) use log_error;

/// Points the log at `dir` and records panics in it. Until this runs (and in
/// tests) `error` writes nothing.
pub fn init(dir: &Path) {
    if PATH.set(dir.join(FILE_NAME)).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error(&format!("panic: {info}"));
        previous(info);
    }));
}

/// Appends one line to the log. A failure to write is only printed: logging
/// must never become a second error.
pub fn error(message: &str) {
    let Some(path) = PATH.get() else {
        return;
    };
    let message = crate::provider::redact_secrets(message);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if is_repeat(&mut last, &message, Instant::now()) {
        return;
    }
    let line = line(&stamp(), &message);
    if let Err(error) = append(path, &line, MAX_BYTES) {
        eprintln!("Could not write {}: {error}", path.display());
    }
}

/// True when `message` is the one just written, less than `REPEAT_WINDOW` ago;
/// otherwise remembers it as the latest.
fn is_repeat(last: &mut Option<(String, Instant)>, message: &str, now: Instant) -> bool {
    match last {
        Some((text, at)) if text == message && now.duration_since(*at) < REPEAT_WINDOW => true,
        _ => {
            *last = Some((message.to_owned(), now));
            false
        }
    }
}

/// Logs a command's `Err` as `<what>: <reason>` and passes the result on.
pub fn logged<T>(what: &str, result: Result<T, String>) -> Result<T, String> {
    if let Err(reason) = &result {
        error(&format!("{what}: {reason}"));
    }
    result
}

/// `<stamp> <message>\n`, on one line and bounded.
fn line(stamp: &str, message: &str) -> String {
    let flat: String = message
        .chars()
        .take(MAX_MESSAGE)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("{stamp} {}\n", flat.trim_end())
}

#[cfg(windows)]
fn stamp() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut now = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    // SAFETY: a valid, writable SYSTEMTIME.
    unsafe { GetLocalTime(&mut now) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    )
}

#[cfg(not(windows))]
fn stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("@{seconds}")
}

fn append(path: &Path, line: &str, max_bytes: u64) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true);
    let full = fs::metadata(path).is_ok_and(|meta| meta.len() >= max_bytes);
    let mut older = path.as_os_str().to_owned();
    older.push(".1");
    // Rename replaces an existing target on Windows too. If it is refused (a
    // viewer holds parla.log.1 open), start the log over rather than lose the
    // line or let the file grow without bound.
    if full && fs::rename(path, older).is_err() {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    let mut file = match options.open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            options.open(path)?
        }
        other => other?,
    };
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_one_stamped_row_without_control_characters() {
        assert_eq!(
            line("2026-10-20 18:00:00", "boom\r\nsecond\tline  \n"),
            "2026-10-20 18:00:00 boom  second line\n"
        );
    }

    #[test]
    fn a_runaway_message_is_cut() {
        let long = "x".repeat(MAX_MESSAGE * 3);
        assert_eq!(line("t", &long).len(), "t ".len() + MAX_MESSAGE + 1);
    }

    #[test]
    fn appends_in_order_and_creates_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE_NAME);
        append(&path, "one\n", MAX_BYTES).unwrap();
        append(&path, "two\n", MAX_BYTES).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn a_full_log_moves_to_dot_one_and_replaces_the_older_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let older = dir.path().join("parla.log.1");
        append(&path, "aaaa\n", 10).unwrap();
        append(&path, "bbbb\n", 10).unwrap();
        assert!(
            !older.exists(),
            "10 bytes is exactly the limit, not past it"
        );
        append(&path, "cccc\n", 10).unwrap();
        assert_eq!(fs::read_to_string(&older).unwrap(), "aaaa\nbbbb\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "cccc\n");
        append(&path, "dddd\n", 10).unwrap();
        append(&path, "eeee\n", 10).unwrap();
        assert_eq!(fs::read_to_string(&older).unwrap(), "cccc\ndddd\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "eeee\n");
    }

    #[test]
    fn a_refused_rotation_starts_the_log_over_instead_of_dropping_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        // A directory in the way makes the rename fail on every platform.
        fs::create_dir(dir.path().join("parla.log.1")).unwrap();
        fs::write(&path, "0123456789").unwrap();
        append(
            &path, "new
", 10,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "new
"
        );
    }

    #[test]
    fn an_identical_message_is_held_back_for_a_minute() {
        let start = Instant::now();
        let mut last = None;
        assert!(!is_repeat(&mut last, "mic gone", start));
        assert!(is_repeat(
            &mut last,
            "mic gone",
            start + Duration::from_secs(59)
        ));
        assert!(!is_repeat(
            &mut last,
            "other",
            start + Duration::from_secs(60)
        ));
        assert!(!is_repeat(
            &mut last,
            "mic gone",
            start + Duration::from_secs(61)
        ));
        assert!(!is_repeat(
            &mut last,
            "mic gone",
            start + Duration::from_secs(121)
        ));
    }

    #[test]
    fn without_init_error_writes_nothing() {
        // `init` is never called by the tests, so this must be a quiet no-op.
        error("not written");
    }

    #[cfg(windows)]
    #[test]
    fn the_stamp_reads_as_a_local_date_and_time() {
        let stamp = stamp();
        assert_eq!(stamp.len(), 19, "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], " ");
        assert_eq!(&stamp[13..14], ":");
    }
}
