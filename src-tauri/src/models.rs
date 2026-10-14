//! Parakeet V3 int8 export used by transcribe-rs/Handy, verified 2026-10-15:
//! https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx
//! https://github.com/cjpais/transcribe-rs#model-downloads
//! Downloads never become visible to the provider until every file is verified.
use crate::{provider::parakeet::ParakeetConfig, settings::Settings};
use reqwest::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};
use tauri::Emitter;

pub const ID: &str = "parakeet";
pub const DIRECTORY: &str = "parakeet-tdt-0.6b-v3";
pub const SOURCE: &str = "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce";
pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}
pub const FILES: &[ModelFile] = &[
    ModelFile {
        name: "encoder-model.int8.onnx",
        size: 652_183_999,
        sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
    },
    ModelFile {
        name: "decoder_joint-model.int8.onnx",
        size: 18_202_004,
        sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
    },
    ModelFile {
        name: "nemo128.onnx",
        size: 139_764,
        sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
    },
    ModelFile {
        name: "vocab.txt",
        size: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    },
];
static DEFAULT_DIR: OnceLock<PathBuf> = OnceLock::new();
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

pub fn setup(app: &tauri::AppHandle, settings: &Settings) -> Result<(), String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("models")
        .join(DIRECTORY);
    DEFAULT_DIR
        .set(dir)
        .map_err(|_| "Model directory already initialized")?;
    if settings.active_provider == ID {
        let settings = settings.clone();
        tauri::async_runtime::spawn_blocking(move || {
            // A missing model stays unavailable; startup is never blocked by inference.
            if let Err(error) = crate::provider::parakeet::warm(&settings) {
                eprintln!("Parakeet warm-up: {error}");
            }
        });
    }
    Ok(())
}

pub fn model_dir(settings: &Settings) -> Result<PathBuf, String> {
    let config: ParakeetConfig = settings.provider_config(ID);
    config
        .model_dir
        .or_else(|| DEFAULT_DIR.get().cloned())
        .ok_or_else(|| "App data directory not initialized".into())
}

pub fn available(dir: &Path) -> bool {
    FILES.iter().all(|file| {
        fs::metadata(dir.join(file.name)).is_ok_and(|m| m.is_file() && m.len() == file.size)
    })
}
fn verified_files(dir: &Path, files: &[ModelFile]) -> bool {
    files.iter().all(|file| {
        File::open(dir.join(file.name))
            .is_ok_and(|input| copy_verified(input, &mut std::io::sink(), file, |_| {}).is_ok())
    })
}
fn needs_download(dir: &Path, files: &[ModelFile], repair: bool) -> bool {
    let complete = files.iter().all(|file| {
        fs::metadata(dir.join(file.name)).is_ok_and(|m| m.is_file() && m.len() == file.size)
    });
    !complete || (repair && !verified_files(dir, files))
}
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(30);
fn model_client(stall: Duration) -> Result<Client, String> {
    // Async reqwest exposes a per-read timeout; the blocking builder does not.
    // https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html#method.read_timeout
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(stall)
        .timeout(Duration::from_secs(60 * 60))
        .user_agent(concat!("parla/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}
fn transport_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "Model download stalled or timed out; retry the download".into()
    } else {
        error.without_url().to_string()
    }
}

/// Drives timed async body reads from the existing blocking IO worker. The
/// bounded pending buffer preserves chunks larger than the caller's read buffer.
struct ResponseReader {
    response: reqwest::Response,
    pending: std::io::Cursor<Vec<u8>>,
}
impl Read for ResponseReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let copied = self.pending.read(output)?;
            if copied != 0 {
                return Ok(copied);
            }
            let chunk = tauri::async_runtime::block_on(self.response.chunk())
                .map_err(|e| std::io::Error::other(transport_error(e)))?;
            match chunk {
                Some(bytes) => self.pending = std::io::Cursor::new(bytes.to_vec()),
                None => return Ok(0),
            }
        }
    }
}
fn check_id(id: &str) -> Result<(), String> {
    if id == ID {
        Ok(())
    } else {
        Err(format!("Unknown model id: {id}"))
    }
}
#[derive(Debug, Serialize)]
pub struct ModelStatus {
    pub id: &'static str,
    pub model_dir: PathBuf,
    pub available: bool,
    pub downloading: bool,
}
pub fn status(id: &str, settings: &Settings) -> Result<ModelStatus, String> {
    check_id(id)?;
    let dir = model_dir(settings)?;
    Ok(ModelStatus {
        id: ID,
        available: available(&dir),
        model_dir: dir,
        downloading: DOWNLOADING.load(Ordering::Acquire),
    })
}

#[derive(Clone, Serialize)]
pub struct Progress {
    pub id: &'static str,
    pub downloaded: u64,
    pub total: u64,
}
#[derive(Clone, Serialize)]
pub struct Done {
    pub id: &'static str,
}
#[derive(Clone, Serialize)]
pub struct DownloadError {
    pub id: &'static str,
    pub error: String,
}

struct DownloadGuard<'a>(&'a AtomicBool);
impl<'a> DownloadGuard<'a> {
    fn acquire(busy: &'a AtomicBool) -> Result<Self, String> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "Model download already in progress")?;
        Ok(Self(busy))
    }
}
impl Drop for DownloadGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Runs on a blocking worker. The invoke resolves only after the done/error event.
pub fn download(
    app: &tauri::AppHandle,
    id: &str,
    settings: &Settings,
    repair: bool,
) -> Result<(), String> {
    check_id(id)?;
    let guard = DownloadGuard::acquire(&DOWNLOADING)?;
    let result: Result<(), String> = (|| {
        let target = model_dir(settings)?;
        if needs_download(&target, FILES, repair) {
            let client = model_client(READ_STALL_TIMEOUT)?;
            fetch(&client, &target, |progress| {
                let _ = app.emit("model://progress", progress);
            })?;
        }
        Ok(())
    })();
    // Status listeners observing a terminal event must see downloading=false.
    drop(guard);
    match &result {
        Ok(()) => {
            let _ = app.emit("model://done", Done { id: ID });
        }
        Err(error) => {
            let _ = app.emit(
                "model://error",
                DownloadError {
                    id: ID,
                    error: crate::provider::redact_secrets(error),
                },
            );
        }
    }
    result
}

fn fetch(client: &Client, target: &Path, progress: impl FnMut(Progress)) -> Result<(), String> {
    fetch_from(client, target, FILES, SOURCE, progress)
}

fn fetch_from(
    client: &Client,
    target: &Path,
    files: &[ModelFile],
    source: &str,
    mut progress: impl FnMut(Progress),
) -> Result<(), String> {
    let total = files.iter().map(|f| f.size).sum();
    let mut downloaded = 0;
    let mut last_event = Instant::now();
    progress(Progress {
        id: ID,
        downloaded,
        total,
    });
    install(target, |staging| {
        for file in files {
            let response = tauri::async_runtime::block_on(async {
                client.get(format!("{source}/{}", file.name)).send().await
            })
            .and_then(|r| r.error_for_status())
            .map_err(transport_error)?;
            let response = ResponseReader {
                response,
                pending: std::io::Cursor::new(Vec::new()),
            };
            let mut output = File::create(staging.join(file.name)).map_err(|e| e.to_string())?;
            copy_verified(response, &mut output, file, |bytes| {
                downloaded += bytes;
                if last_event.elapsed() >= Duration::from_millis(100) {
                    progress(Progress {
                        id: ID,
                        downloaded,
                        total,
                    });
                    last_event = Instant::now();
                }
            })?;
            output.sync_all().map_err(|e| e.to_string())?;
            progress(Progress {
                id: ID,
                downloaded,
                total,
            });
        }
        Ok(())
    })
}

fn copy_verified(
    mut input: impl Read,
    output: &mut impl Write,
    file: &ModelFile,
    mut progress: impl FnMut(u64),
) -> Result<(), String> {
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if bytes == 0 {
            break;
        }
        count += bytes as u64;
        if count > file.size {
            return Err(format!("Incorrect size for {}", file.name));
        }
        output
            .write_all(&buffer[..bytes])
            .map_err(|e| e.to_string())?;
        hash.update(&buffer[..bytes]);
        progress(bytes as u64);
    }
    if count != file.size || format!("{:x}", hash.finalize()) != file.sha256 {
        return Err(format!("Size/hash verification failed for {}", file.name));
    }
    Ok(())
}

/// Same-filesystem staging and rename. Preserve an incomplete/manual directory
/// until replacement is ready, and restore it if publishing fails (Windows too).
fn install(
    target: &Path,
    populate: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("Model directory needs a parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let staging = tempfile::Builder::new()
        .prefix(".parla-model-")
        .tempdir_in(parent)
        .map_err(|e| e.to_string())?;
    populate(staging.path())?;
    // A private sibling temp directory holds the old version during publication.
    let backup = tempfile::Builder::new()
        .prefix(".parla-backup-")
        .tempdir_in(parent)
        .map_err(|e| e.to_string())?;
    let previous = backup.path().join("previous");
    let had_previous = target.exists();
    if had_previous {
        fs::rename(target, &previous).map_err(|e| e.to_string())?;
    }
    if let Err(error) = fs::rename(staging.path(), target) {
        if had_previous {
            if let Err(restore) = fs::rename(&previous, target) {
                let kept = backup.keep();
                return Err(format!("Publishing model failed: {error}; restore failed: {restore}; previous files retained at {}", kept.display()));
            }
        }
        return Err(error.to_string());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn small_file() -> ModelFile {
        ModelFile {
            name: "small",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        }
    }
    // A local HTTP server exercises the real transport without downloading a model.
    fn server(
        body: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            use std::io::BufRead;
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            body(stream);
            headers
        });
        (url, worker)
    }
    #[test]
    fn ordinary_download_skips_hashes_but_explicit_repair_detects_corruption() {
        let root = tempfile::tempdir().unwrap();
        let spec = small_file();
        let files = std::slice::from_ref(&spec);
        assert!(needs_download(root.path(), files, false));
        std::fs::write(root.path().join("small"), b"abd").unwrap();
        assert!(!needs_download(root.path(), files, false));
        assert!(needs_download(root.path(), files, true));
        std::fs::write(root.path().join("small"), b"abc").unwrap();
        assert!(!needs_download(root.path(), files, true));
    }
    #[test]
    fn mid_body_stall_cleans_staging_and_releases_busy_flag() {
        let (release, wait) = std::sync::mpsc::channel();
        let (source, worker) = server(move |mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\na")
                .unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(3));
        });
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("model");
        let started = Instant::now();
        let client = model_client(Duration::from_millis(100)).unwrap();
        let busy = AtomicBool::new(false);
        let result = {
            let _guard = DownloadGuard::acquire(&busy).unwrap();
            fetch_from(&client, &target, &[small_file()], &source, |_| {})
        };
        assert!(!busy.load(Ordering::Acquire));
        assert!(DownloadGuard::acquire(&busy).is_ok());
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(result.unwrap_err().contains("stalled"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn read_timeout_resets_when_bytes_arrive_and_user_agent_is_sent() {
        let (source, worker) = server(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n")
                .unwrap();
            for byte in b"abc" {
                stream.write_all(&[*byte]).unwrap();
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("model");
        let client = model_client(Duration::from_millis(180)).unwrap();
        let result = fetch_from(&client, &target, &[small_file()], &source, |_| {});
        let headers = worker.join().unwrap();
        result.unwrap();
        assert!(headers
            .to_lowercase()
            .contains(&format!("user-agent: parla/{}", env!("CARGO_PKG_VERSION"))));
        assert_eq!(std::fs::read(target.join("small")).unwrap(), b"abc");
    }
    #[test]
    fn installed_files_are_verified_before_skipping_download() {
        let root = tempfile::tempdir().unwrap();
        let spec = ModelFile {
            name: "small",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        assert!(!verified_files(root.path(), std::slice::from_ref(&spec)));
        std::fs::write(root.path().join("small"), b"abd").unwrap();
        assert!(!verified_files(root.path(), std::slice::from_ref(&spec)));
        std::fs::write(root.path().join("small"), b"abc").unwrap();
        assert!(verified_files(root.path(), std::slice::from_ref(&spec)));
    }
    #[test]
    fn rejects_corrupt_truncated_and_oversized_files() {
        let spec = ModelFile {
            name: "small",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let mut output = Vec::new();
        let mut progress = 0;
        copy_verified(&b"abc"[..], &mut output, &spec, |n| progress += n).unwrap();
        assert_eq!(output, b"abc");
        assert_eq!(progress, 3);
        for bad in [&b"ab"[..], &b"abd"[..], &b"abcd"[..]] {
            assert!(copy_verified(bad, &mut Vec::new(), &spec, |_| {}).is_err());
        }
    }
    #[test]
    fn status_and_event_payloads_match_ipc_contract() {
        let root = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings
            .providers
            .insert(ID.into(), serde_json::json!({"model_dir":root.path()}));
        assert!(status("../other", &settings).is_err());
        let status = status(ID, &settings).unwrap();
        assert!(!status.available);
        assert_eq!(status.model_dir, root.path());
        assert_eq!(
            serde_json::to_value(Progress {
                id: ID,
                downloaded: 1,
                total: 3
            })
            .unwrap(),
            serde_json::json!({"id":"parakeet","downloaded":1,"total":3})
        );
        assert_eq!(
            serde_json::to_value(Done { id: ID }).unwrap(),
            serde_json::json!({"id":"parakeet"})
        );
        assert_eq!(
            serde_json::to_value(DownloadError {
                id: ID,
                error: "offline".into()
            })
            .unwrap(),
            serde_json::json!({"id":"parakeet","error":"offline"})
        );
    }
    #[test]
    fn concurrent_download_is_rejected_and_guard_resets() {
        let busy = AtomicBool::new(false);
        let guard = DownloadGuard::acquire(&busy).unwrap();
        assert!(DownloadGuard::acquire(&busy).is_err());
        drop(guard);
        assert!(DownloadGuard::acquire(&busy).is_ok());
    }
    #[test]
    fn live_download_only_when_explicitly_requested() {
        let Ok(dir) = std::env::var("PARLA_PARAKEET_DOWNLOAD_DIR") else {
            return;
        };
        let mut last = 0;
        fetch(
            &model_client(READ_STALL_TIMEOUT).unwrap(),
            Path::new(&dir),
            |event| {
                assert!(event.downloaded >= last);
                assert!(event.downloaded <= event.total);
                last = event.downloaded;
            },
        )
        .unwrap();
        assert!(available(Path::new(&dir)));
        assert_eq!(last, FILES.iter().map(|f| f.size).sum::<u64>());
    }
    #[test]
    fn publishes_only_completed_download_and_cleans_failure() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("model");
        assert!(install(&target, |staging| {
            assert!(!target.exists());
            std::fs::write(staging.join("small"), b"complete").unwrap();
            Ok(())
        })
        .is_ok());
        assert_eq!(std::fs::read(target.join("small")).unwrap(), b"complete");
        let failed = root.path().join("failed");
        assert!(install(&failed, |staging| {
            std::fs::write(staging.join("partial"), b"partial").unwrap();
            Err("connection lost".into())
        })
        .is_err());
        assert!(!failed.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
    #[test]
    fn failed_redownload_preserves_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("model");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("old"), b"preserved").unwrap();
        assert!(install(&target, |_| Err("offline".into())).is_err());
        assert_eq!(std::fs::read(target.join("old")).unwrap(), b"preserved");
        install(&target, |staging| {
            std::fs::write(staging.join("new"), b"new").unwrap();
            Ok(())
        })
        .unwrap();
        assert!(target.join("new").exists());
        assert!(!target.join("old").exists());
    }
}
