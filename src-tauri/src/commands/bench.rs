//! Benchmark IPC (PARLA-10). Recording and runs happen on blocking workers.
//! Both hold the `DictationState` guard, so a hotkey dictation cannot start
//! (or paste) while the benchmark owns the microphone or the providers.
use crate::{
    bench::{self, BenchRun, BenchSession},
    clip::Clip,
    flow::DictationState,
    history::History,
    provider::registry,
    settings::SharedSettings,
};
use serde::Serialize;
use std::time::Duration;
use tauri::{Emitter, Manager};

/// The recorder stops itself this long after "Record clip" so a forgotten
/// recording cannot hold the microphone and the dictation guard forever.
const MAX_RECORDING: Duration = Duration::from_secs(30);

#[derive(Serialize, Clone)]
pub struct ClipInfo {
    pub audio_ms: u64,
}

/// `bench://recording-stopped` payload for the automatic 30 s stop.
#[derive(Serialize, Clone)]
struct AutoStopped {
    audio_ms: Option<u64>,
    error: Option<String>,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn bench_start_recording(app: tauri::AppHandle) -> Result<(), String> {
    let generation = app
        .state::<BenchSession>()
        .window_generation()
        .ok_or("Benchmark window is closed")?;
    blocking(move || start_recording(&app, generation)).await
}

fn start_recording(app: &tauri::AppHandle, generation: u64) -> Result<(), String> {
    let session = app.state::<BenchSession>();
    if session.window_generation() != Some(generation) {
        return Err("Benchmark window was closed".into());
    }
    if session.is_recording() {
        return Err("Already recording".into());
    }
    let guard = app
        .state::<DictationState>()
        .try_begin()
        .ok_or("Dictation in progress")?;
    let recorder = crate::recorder::recorder().ok_or("No microphone available")?;
    recorder
        .start()
        .map_err(|error| format!("Recording failed: {error:#}"))?;
    let token = match session.begin_recording(guard, generation) {
        Ok(token) => token,
        Err(guard) => {
            let _guard = guard;
            recorder.stop();
            return Err("Benchmark window was closed".into());
        }
    };
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(MAX_RECORDING);
        if let Some(outcome) = finish_recording(&app, Some(token)) {
            let payload = match outcome {
                Ok(audio_ms) => AutoStopped {
                    audio_ms: Some(audio_ms),
                    error: None,
                },
                Err(error) => AutoStopped {
                    audio_ms: None,
                    error: Some(error),
                },
            };
            let _ = app.emit("bench://recording-stopped", payload);
        }
    });
    Ok(())
}

/// Stops the recording identified by `token` (any when `None`) and stores the
/// clip. `None` means there was no such recording to stop.
pub(crate) fn finish_recording(
    app: &tauri::AppHandle,
    token: Option<u64>,
) -> Option<Result<u64, String>> {
    let session = app.state::<BenchSession>();
    // Keep the guard until the recorder has stopped and the clip is stored.
    let _guard = session.take_recording(token)?;
    let clip = crate::recorder::recorder()
        .map(|recorder| recorder.stop())
        .unwrap_or_else(|| Clip::new(Vec::new()));
    if clip.samples.is_empty() {
        return Some(Err("No audio captured".into()));
    }
    let audio_ms = clip.duration().as_millis() as u64;
    match app.path().app_data_dir() {
        Ok(dir) => {
            let name = format!(
                "bench-{}.wav",
                crate::timing::DictationRecord::new(std::time::SystemTime::now(), "").id
            );
            if let Err(error) = clip.save_wav(&dir.join("clips").join(name)) {
                eprintln!("bench: saving the clip failed: {error:#}");
            }
        }
        Err(error) => eprintln!("bench: resolving the app data dir failed: {error}"),
    }
    session.set_clip(clip);
    Some(Ok(audio_ms))
}

#[tauri::command]
pub async fn bench_stop_recording(app: tauri::AppHandle) -> Result<ClipInfo, String> {
    blocking(move || {
        finish_recording(&app, None)
            .ok_or_else(|| "Not recording".to_string())?
            .map(|audio_ms| ClipInfo { audio_ms })
    })
    .await
}

/// The clip a run would use, if one was recorded since startup.
#[tauri::command]
pub fn bench_clip(session: tauri::State<'_, BenchSession>) -> Option<ClipInfo> {
    session.clip().map(|clip| ClipInfo {
        audio_ms: clip.duration().as_millis() as u64,
    })
}

#[tauri::command]
pub async fn bench_run(app: tauri::AppHandle) -> Result<BenchRun, String> {
    blocking(move || {
        let session = app.state::<BenchSession>();
        if session.is_recording() {
            return Err("Stop recording first".to_string());
        }
        let clip = session.clip().ok_or("Record a clip first")?;
        let _guard = app
            .state::<DictationState>()
            .try_begin()
            .ok_or("Dictation in progress")?;
        let settings = app
            .state::<SharedSettings>()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let history = app.try_state::<History>();
        Ok(bench::run_with_providers(
            &clip,
            &settings,
            registry::all(),
            |progress| {
                let _ = app.emit("bench://progress", progress);
            },
            |row| {
                crate::stats::record(&app, &row);
                if let Some(history) = &history {
                    history.append(row);
                }
            },
        ))
    })
    .await
}

/// Most recent runs, rebuilt from history rows (newest first).
#[tauri::command]
pub async fn bench_runs(
    app: tauri::AppHandle,
    limit: Option<usize>,
) -> Result<Vec<BenchRun>, String> {
    let limit = limit.unwrap_or(20).min(100);
    blocking(move || {
        let rows = app
            .try_state::<History>()
            .map(|history| history.load_recent(crate::history::MAX_LIMIT))
            .unwrap_or_default();
        Ok(bench::group_runs(rows, limit))
    })
    .await
}
