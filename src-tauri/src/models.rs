//! Local Parakeet exports, all in the istupakov ONNX layout that transcribe-rs
//! loads (int8 encoder and decoder, nemo128 preprocessor, vocab). Verified 2026-10-15/16:
//! v3: https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx
//!     https://github.com/cjpais/transcribe-rs#model-downloads
//! Ultra: https://huggingface.co/Masterx/parakeet-tdt-0.6b-ultra-onnx (CC BY 4.0),
//!     moondream/parakeet-ultra weights in the v3 graphs; its int8 files use the
//!     same quantize_dynamic recipe as v3. The repo ships no preprocessor, so
//!     nemo128.onnx (no trained weights) comes from the pinned v3 revision.
//! Downloads never become visible to the provider until every file is verified.
use crate::log::log_error;
use crate::{
    provider::parakeet::ParakeetConfig,
    settings::{LocalModel, Settings},
};
use reqwest::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};
use tauri::Emitter;

/// The provider id; the models below are its interchangeable weights.
pub const ID: &str = "parakeet";
const V3_SOURCE: &str = "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce";
const ULTRA_SOURCE: &str = "https://huggingface.co/Masterx/parakeet-tdt-0.6b-ultra-onnx/resolve/99b09f030a5a6efeaa13cf2cf54592100ce2c3f1";
pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// Base URL; the file name is appended.
    pub source: &'static str,
}
const NEMO128: ModelFile = ModelFile {
    name: "nemo128.onnx",
    size: 139_764,
    sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
    source: V3_SOURCE,
};
pub struct ModelSpec {
    pub model: LocalModel,
    pub name: &'static str,
    /// Folder under the app's models directory.
    pub directory: &'static str,
    /// Recorded in history, so each dictation shows which weights produced it.
    pub history_name: &'static str,
    pub files: &'static [ModelFile],
}
pub const V3: ModelSpec = ModelSpec {
    model: LocalModel::ParakeetV3,
    name: "Parakeet v3",
    directory: "parakeet-tdt-0.6b-v3",
    history_name: "parakeet-tdt-0.6b-v3",
    files: &[
        ModelFile {
            name: "encoder-model.int8.onnx",
            size: 652_183_999,
            sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
            source: V3_SOURCE,
        },
        ModelFile {
            name: "decoder_joint-model.int8.onnx",
            size: 18_202_004,
            sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
            source: V3_SOURCE,
        },
        NEMO128,
        ModelFile {
            name: "vocab.txt",
            size: 93_939,
            sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
            source: V3_SOURCE,
        },
    ],
};
pub const ULTRA: ModelSpec = ModelSpec {
    model: LocalModel::ParakeetUltra,
    name: "Parakeet Ultra",
    directory: "parakeet-ultra-onnx-int8",
    history_name: "parakeet-ultra-onnx-int8",
    files: &[
        ModelFile {
            name: "encoder-model.int8.onnx",
            size: 652_183_214,
            sha256: "46e78f85f1ae43b43bd359889c966da5c7ca401df60a42eeffc2e7699b357ba6",
            source: ULTRA_SOURCE,
        },
        ModelFile {
            name: "decoder_joint-model.int8.onnx",
            size: 18_202_004,
            sha256: "2276a335d4c8dc48686e931475a634595954956427d485830e1214c0d1a18d07",
            source: ULTRA_SOURCE,
        },
        NEMO128,
        // Byte-identical to v3's: same tokenizer.
        ModelFile {
            name: "vocab.txt",
            size: 93_939,
            sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
            source: ULTRA_SOURCE,
        },
    ],
};
pub fn spec(model: LocalModel) -> &'static ModelSpec {
    match model {
        LocalModel::ParakeetV3 => &V3,
        LocalModel::ParakeetUltra => &ULTRA,
    }
}
static MODELS_ROOT: OnceLock<PathBuf> = OnceLock::new();
/// 0 when idle, else the downloading model's `tag`. One download at a time.
static DOWNLOADING: AtomicU8 = AtomicU8::new(0);
fn tag(model: LocalModel) -> u8 {
    match model {
        LocalModel::ParakeetV3 => 1,
        LocalModel::ParakeetUltra => 2,
    }
}

pub fn setup(app: &tauri::AppHandle, settings: &Settings) -> Result<(), String> {
    use tauri::Manager;
    let root = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("models");
    MODELS_ROOT
        .set(root)
        .map_err(|_| "Model directory already initialized")?;
    if settings.active_provider == ID {
        let settings = settings.clone();
        tauri::async_runtime::spawn_blocking(move || {
            // A missing model stays unavailable; startup is never blocked by inference.
            if let Err(error) = crate::provider::parakeet::warm(&settings) {
                log_error!("Parakeet warm-up: {error}");
            }
        });
    }
    Ok(())
}

/// The selected model's directory.
pub fn model_dir(settings: &Settings) -> Result<PathBuf, String> {
    model_dir_for(settings, settings.local_model)
}

/// A configured directory overrides the default folder of its own model only,
/// so a v3 override never receives (or loses its files to) an Ultra download.
pub fn model_dir_for(settings: &Settings, model: LocalModel) -> Result<PathBuf, String> {
    let config: ParakeetConfig = settings.provider_config(ID);
    let custom = match model {
        LocalModel::ParakeetV3 => config.model_dir,
        LocalModel::ParakeetUltra => config.ultra_model_dir,
    };
    custom
        .or_else(|| {
            MODELS_ROOT
                .get()
                .map(|root| root.join(spec(model).directory))
        })
        .ok_or_else(|| "App data directory not initialized".into())
}

pub fn available(model: LocalModel, dir: &Path) -> bool {
    spec(model).files.iter().all(|file| {
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
fn parse_id(id: &str) -> Result<LocalModel, String> {
    LocalModel::from_id(id).ok_or_else(|| format!("Unknown model id: {id}"))
}
#[derive(Debug, Serialize)]
pub struct ModelStatus {
    pub id: &'static str,
    pub name: &'static str,
    /// Download size in bytes.
    pub size: u64,
    pub model_dir: PathBuf,
    pub available: bool,
    pub downloading: bool,
    pub selected: bool,
}
pub fn status(id: &str, settings: &Settings) -> Result<ModelStatus, String> {
    let model = parse_id(id)?;
    let spec = spec(model);
    let dir = model_dir_for(settings, model)?;
    Ok(ModelStatus {
        id: model.id(),
        name: spec.name,
        size: spec.files.iter().map(|f| f.size).sum(),
        available: available(model, &dir),
        model_dir: dir,
        downloading: DOWNLOADING.load(Ordering::Acquire) == tag(model),
        selected: settings.local_model == model,
    })
}
pub fn list(settings: &Settings) -> Result<Vec<ModelStatus>, String> {
    LocalModel::ALL
        .into_iter()
        .map(|model| status(model.id(), settings))
        .collect()
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

struct DownloadGuard<'a>(&'a AtomicU8);
impl<'a> DownloadGuard<'a> {
    fn acquire(busy: &'a AtomicU8, tag: u8) -> Result<Self, String> {
        busy.compare_exchange(0, tag, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "Model download already in progress")?;
        Ok(Self(busy))
    }
}
impl Drop for DownloadGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

/// Runs on a blocking worker. The invoke resolves only after the done/error event.
pub fn download(
    app: &tauri::AppHandle,
    id: &str,
    settings: &Settings,
    repair: bool,
) -> Result<(), String> {
    let model = parse_id(id)?;
    let guard = DownloadGuard::acquire(&DOWNLOADING, tag(model))?;
    let result: Result<(), String> = (|| {
        let target = model_dir_for(settings, model)?;
        let files = spec(model).files;
        if needs_download(&target, files, repair) {
            let client = model_client(READ_STALL_TIMEOUT)?;
            fetch_from(&client, &target, model.id(), files, |progress| {
                let _ = app.emit("model://progress", progress);
            })?;
        }
        Ok(())
    })();
    // Status listeners observing a terminal event must see downloading=false.
    drop(guard);
    match &result {
        Ok(()) => {
            let _ = app.emit("model://done", Done { id: model.id() });
        }
        Err(error) => {
            let _ = app.emit(
                "model://error",
                DownloadError {
                    id: model.id(),
                    error: crate::provider::redact_secrets(error),
                },
            );
        }
    }
    result
}

/// Bumped by every model selection, so a selection that finishes its download
/// after the user picked another model does not override the later choice.
static SELECTION: AtomicU64 = AtomicU64::new(0);
pub fn begin_selection() -> u64 {
    SELECTION.fetch_add(1, Ordering::AcqRel) + 1
}
pub fn selection_current(selection: u64) -> bool {
    SELECTION.load(Ordering::Acquire) == selection
}

fn fetch_from(
    client: &Client,
    target: &Path,
    id: &'static str,
    files: &[ModelFile],
    mut progress: impl FnMut(Progress),
) -> Result<(), String> {
    let total = files.iter().map(|f| f.size).sum();
    let mut downloaded = 0;
    let mut last_event = Instant::now();
    progress(Progress {
        id,
        downloaded,
        total,
    });
    install(target, |staging| {
        for file in files {
            let response = request(client, file)?;
            let mut output = File::create(staging.join(file.name)).map_err(|e| e.to_string())?;
            copy_verified(response, &mut output, file, |bytes| {
                downloaded += bytes;
                if last_event.elapsed() >= Duration::from_millis(100) {
                    progress(Progress {
                        id,
                        downloaded,
                        total,
                    });
                    last_event = Instant::now();
                }
            })?;
            output.sync_all().map_err(|e| e.to_string())?;
            progress(Progress {
                id,
                downloaded,
                total,
            });
        }
        Ok(())
    })
}

fn request(client: &Client, file: &ModelFile) -> Result<ResponseReader, String> {
    let response = tauri::async_runtime::block_on(async {
        client
            .get(format!("{}/{}", file.source, file.name))
            .send()
            .await
    })
    .and_then(|r| r.error_for_status())
    .map_err(transport_error)?;
    Ok(ResponseReader {
        response,
        pending: std::io::Cursor::new(Vec::new()),
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
        small_file_at("")
    }
    fn small_file_at(source: &str) -> ModelFile {
        ModelFile {
            name: "small",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            source: Box::leak(source.into()),
        }
    }
    // A local HTTP server exercises the real transport without downloading a
    // model. The returned `finish` joins it and gives the request headers.
    fn server(
        body: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> (String, impl FnOnce() -> String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // Open until the join, so the wake-up connect below is never refused.
        let open = listener.try_clone().unwrap();
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
                // EOF or an error: the client gave up (PARLA-90).
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            body(stream);
            headers
        });
        let finish = move || {
            // A client that gave up before connecting left the server in
            // `accept`: this connection ends it.
            let _ = std::net::TcpStream::connect(address);
            let headers = worker.join().unwrap();
            drop(open);
            headers
        };
        (format!("http://{address}"), finish)
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
        let (source, finish) = server(move |mut stream| {
            // Under load the client can give up before the headers are out.
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\na");
            let _ = wait.recv_timeout(Duration::from_secs(3));
        });
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("model");
        let client = model_client(Duration::from_millis(500)).unwrap();
        let busy = AtomicU8::new(0);
        let result = {
            let _guard = DownloadGuard::acquire(&busy, 1).unwrap();
            fetch_from(&client, &target, "test", &[small_file_at(&source)], |_| {})
        };
        assert_eq!(busy.load(Ordering::Acquire), 0);
        assert!(DownloadGuard::acquire(&busy, 2).is_ok());
        release.send(()).unwrap();
        finish();
        // The server holds the connection until released, so only the stall
        // timeout can end the download (PARLA-90: no wall-clock bound).
        assert!(result.unwrap_err().contains("stalled"));
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn read_timeout_resets_when_bytes_arrive_and_user_agent_is_sent() {
        // reqwest times a read only while it waits (PARLA-90). Each byte is
        // sent before the read that takes it, so no timed wait holds a sleep;
        // idling past STALL between reads fails a total or a non-resetting
        // deadline.
        const STALL: Duration = Duration::from_secs(1);
        let (ask, asked) = std::sync::mpsc::channel();
        let (sent, written) = std::sync::mpsc::channel();
        let (source, finish) = server(move |mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\na")
                .unwrap();
            asked.recv().unwrap();
            stream.write_all(b"bc").unwrap();
            sent.send(()).unwrap();
        });
        let file = small_file_at(&source);
        let client = model_client(STALL).unwrap();
        let response = request(&client, &file).unwrap();
        let mut body = Vec::new();
        let mut gate = Some((ask, written));
        copy_verified(response, &mut body, &file, |_| {
            if let Some((ask, written)) = gate.take() {
                ask.send(()).unwrap();
                written.recv().unwrap();
                std::thread::sleep(STALL + Duration::from_millis(100));
            }
        })
        .unwrap();
        let headers = finish();
        assert!(headers
            .to_lowercase()
            .contains(&format!("user-agent: parla/{}", env!("CARGO_PKG_VERSION"))));
        assert_eq!(body, b"abc");
    }
    #[test]
    fn download_publishes_the_verified_file() {
        let (source, finish) = server(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc")
                .unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("model");
        let client = model_client(READ_STALL_TIMEOUT).unwrap();
        let mut downloaded = 0;
        fetch_from(&client, &target, "test", &[small_file_at(&source)], |p| {
            downloaded = p.downloaded
        })
        .unwrap();
        finish();
        assert_eq!(downloaded, 3);
        assert_eq!(std::fs::read(target.join("small")).unwrap(), b"abc");
    }
    #[test]
    fn installed_files_are_verified_before_skipping_download() {
        let root = tempfile::tempdir().unwrap();
        let spec = small_file();
        assert!(!verified_files(root.path(), std::slice::from_ref(&spec)));
        std::fs::write(root.path().join("small"), b"abd").unwrap();
        assert!(!verified_files(root.path(), std::slice::from_ref(&spec)));
        std::fs::write(root.path().join("small"), b"abc").unwrap();
        assert!(verified_files(root.path(), std::slice::from_ref(&spec)));
    }
    #[test]
    fn rejects_corrupt_truncated_and_oversized_files() {
        let spec = small_file();
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
        assert!(status(ID, &settings).is_err());
        let status = status("parakeet-v3", &settings).unwrap();
        assert!(!status.available);
        assert!(status.selected);
        assert_eq!(status.model_dir, root.path());
        let total: u64 = V3.files.iter().map(|f| f.size).sum();
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({"id":"parakeet-v3","name":"Parakeet v3","size":total,
                "model_dir":root.path(),"available":false,"downloading":false,"selected":true})
        );
        assert_eq!(
            serde_json::to_value(Progress {
                id: "parakeet-ultra",
                downloaded: 1,
                total: 3
            })
            .unwrap(),
            serde_json::json!({"id":"parakeet-ultra","downloaded":1,"total":3})
        );
        assert_eq!(
            serde_json::to_value(Done { id: "parakeet-v3" }).unwrap(),
            serde_json::json!({"id":"parakeet-v3"})
        );
        assert_eq!(
            serde_json::to_value(DownloadError {
                id: "parakeet-v3",
                error: "offline".into()
            })
            .unwrap(),
            serde_json::json!({"id":"parakeet-v3","error":"offline"})
        );
    }
    #[test]
    fn each_model_has_its_own_directory_and_files() {
        let v3 = tempfile::tempdir().unwrap();
        let ultra = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.providers.insert(
            ID.into(),
            serde_json::json!({"model_dir":v3.path(),"ultra_model_dir":ultra.path()}),
        );
        assert_eq!(model_dir(&settings).unwrap(), v3.path());
        settings.local_model = LocalModel::ParakeetUltra;
        assert_eq!(model_dir(&settings).unwrap(), ultra.path());
        let listed = list(&settings).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|s| (s.id, s.selected))
                .collect::<Vec<_>>(),
            [("parakeet-v3", false), ("parakeet-ultra", true)]
        );
        // A v3 override must not redirect Ultra into it (an Ultra install would
        // replace the v3 files there).
        settings
            .providers
            .insert(ID.into(), serde_json::json!({"model_dir":v3.path()}));
        assert_ne!(
            model_dir_for(&settings, LocalModel::ParakeetUltra).ok(),
            Some(v3.path().to_path_buf())
        );
        assert_ne!(V3.directory, ULTRA.directory);
        for (left, right) in V3.files.iter().zip(ULTRA.files) {
            assert_eq!(left.name, right.name, "transcribe-rs expects the v3 layout");
        }
        assert_ne!(V3.files[0].sha256, ULTRA.files[0].sha256);
        for file in V3.files.iter().chain(ULTRA.files) {
            assert!(file.source.contains("/resolve/") && !file.source.ends_with("/main"));
            assert_eq!(file.sha256.len(), 64);
        }
    }
    #[test]
    fn concurrent_download_is_rejected_and_guard_resets() {
        let busy = AtomicU8::new(0);
        let guard = DownloadGuard::acquire(&busy, tag(LocalModel::ParakeetUltra)).unwrap();
        assert_eq!(busy.load(Ordering::Acquire), 2);
        assert!(DownloadGuard::acquire(&busy, tag(LocalModel::ParakeetV3)).is_err());
        drop(guard);
        assert!(DownloadGuard::acquire(&busy, tag(LocalModel::ParakeetV3)).is_ok());
    }
    #[test]
    fn a_later_selection_supersedes_an_earlier_one() {
        let first = begin_selection();
        assert!(selection_current(first));
        let second = begin_selection();
        assert!(!selection_current(first));
        assert!(selection_current(second));
    }
    #[test]
    fn live_download_only_when_explicitly_requested() {
        for (variable, spec) in [
            ("PARLA_PARAKEET_DOWNLOAD_DIR", &V3),
            ("PARLA_PARAKEET_ULTRA_DOWNLOAD_DIR", &ULTRA),
        ] {
            let Ok(dir) = std::env::var(variable) else {
                continue;
            };
            let mut last = 0;
            fetch_from(
                &model_client(READ_STALL_TIMEOUT).unwrap(),
                Path::new(&dir),
                spec.model.id(),
                spec.files,
                |event| {
                    assert!(event.downloaded >= last);
                    assert!(event.downloaded <= event.total);
                    last = event.downloaded;
                },
            )
            .unwrap();
            assert!(available(spec.model, Path::new(&dir)));
            assert!(verified_files(Path::new(&dir), spec.files));
            assert_eq!(last, spec.files.iter().map(|f| f.size).sum::<u64>());
        }
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
