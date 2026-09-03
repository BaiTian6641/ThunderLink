//! ThunderLink desktop app: Tauri shell over `thunderlink-engine`.
//!
//! UI CONTRACT (invoked from the webview; see src/ frontend):
//!   get_status() -> { running: bool, role: "target"|"initiator"|null }
//!   get_permissions() -> { screen_recording: bool, accessibility: bool,
//!                          platform: string }
//!   list_targets(timeout_secs) -> [{ name, addrs: [string], port }]
//!   start_target({ windowed, no_input }) -> ok / Err(String)
//!   start_initiator({ addr?, discover, source, codec?, bitrate_kbps?,
//!                     fps?, res?, virtual_display }) -> ok / Err(String)
//!   stop_session() -> ok
//! Events (channel "engine://event"): JSON of thunderlink_engine::
//! EngineEvent — externally tagged serde enum:
//!   {"Negotiated":{...StreamConfig}} | {"Streaming":null}
//!   {"Stats":{...StatsReport}} | {"LatencyMs":12.3}
//!   {"Ended":"reason"} | {"Warn":"message"}
//! Events (channel "engine://state"): { running: bool, role: ... }

use std::net::SocketAddr;
use parking_lot::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use thunderlink_engine::{
    announce_target, browse_targets, discover_target, run_initiator, run_target, CancelToken,
    EngineEvent, EventSink, InitiatorConfig, Source, TargetConfig, AudioSource,
};
#[cfg(target_os = "macos")]
use thunderlink_engine::EmbeddedPresenter;
use tl_proto::CONTROL_PORT;

/// One live role execution (the app is single-session).
struct Session {
    role: &'static str,
    cancel: CancelToken,
}

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Session>>,
}

#[derive(Serialize, Clone)]
struct Status {
    running: bool,
    role: Option<&'static str>,
}

#[derive(Serialize, Clone)]
struct Permissions {
    screen_recording: bool,
    accessibility: bool,
    platform: String,
}

#[derive(Serialize, Clone)]
struct TargetInfo {
    name: String,
    addrs: Vec<String>,
    port: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitiatorOptions {
    /// "host[:port]"; with discover=true this is ignored.
    addr: Option<String>,
    discover: bool,
    /// "test-pattern" | "screen"
    source: String,
    /// "hevc" | "h264"
    codec: Option<String>,
    bitrate_kbps: Option<u32>,
    fps: Option<u32>,
    /// "WxH"
    res: Option<String>,
    virtual_display: bool,
    /// Audio: None/"off" | "sine" | "system"
    audio: Option<String>,
    audio_freq_hz: Option<f64>,
}

fn status_of(state: &AppState) -> Status {
    let g = state.session.lock();
    Status {
        running: g.is_some(),
        role: g.as_ref().map(|s| s.role),
    }
}

fn emit_state(app: &AppHandle, st: Status) {
    let _ = app.emit("engine://state", st);
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> Status {
    status_of(&state)
}

#[tauri::command]
fn get_permissions() -> Permissions {
    Permissions {
        screen_recording: preflight_screen_recording(),
        accessibility: accessibility_trusted(),
        platform: std::env::consts::OS.to_string(),
    }
}

#[cfg(target_os = "macos")]
mod tcc {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(
            options: core_foundation_sys::dictionary::CFDictionaryRef,
        ) -> bool;
        // Framework CFSTR constant (HIServices): option key that makes the
        // trust check show the Accessibility consent prompt.
        static kAXTrustedCheckOptionPrompt: core_foundation_sys::string::CFStringRef;
    }

    pub fn screen_recording() -> bool {
        // SAFETY: nullary C function, no preconditions.
        unsafe { CGPreflightScreenCaptureAccess() }
    }
    pub fn accessibility() -> bool {
        // SAFETY: nullary C function, no preconditions.
        unsafe { AXIsProcessTrusted() }
    }

    /// Show the system consent prompts for any missing grants (first-run
    /// flow). Screen Recording via CGRequestScreenCaptureAccess; Accessibility
    /// via AXIsProcessTrustedWithOptions with the prompt option. macOS will
    /// not re-prompt once the user has explicitly denied — the Settings deep
    /// links remain the fallback.
    pub fn request_missing() {
        if !screen_recording() {
            // SAFETY: nullary; shows the Screen Recording prompt if allowed.
            unsafe { CGRequestScreenCaptureAccess() };
        }
        if !accessibility() {
            // SAFETY: dictionary built from framework constants below; the
            // option key/value are valid CF objects owned by the dict.
            unsafe {
                let key: core_foundation_sys::string::CFStringRef = kAXTrustedCheckOptionPrompt;
                let value = core_foundation_sys::number::kCFBooleanTrue;
                let options = core_foundation_sys::dictionary::CFDictionaryCreate(
                    core_foundation_sys::base::kCFAllocatorDefault,
                    &key as *const _ as *const core_foundation_sys::base::CFTypeRef,
                    &value as *const _ as *const core_foundation_sys::base::CFTypeRef,
                    1,
                    &core_foundation_sys::dictionary::kCFTypeDictionaryKeyCallBacks,
                    &core_foundation_sys::dictionary::kCFTypeDictionaryValueCallBacks,
                );
                if !options.is_null() {
                    AXIsProcessTrustedWithOptions(options);
                    core_foundation_sys::base::CFRelease(options as *const _);
                }
            };
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
mod tcc {
    pub fn screen_recording() -> bool {
        true
    }
    pub fn accessibility() -> bool {
        true
    }
    pub fn request_missing() {}
}

#[cfg(target_os = "macos")]
fn preflight_screen_recording() -> bool {
    tcc::screen_recording()
}
#[cfg(not(target_os = "macos"))]
fn preflight_screen_recording() -> bool {
    true
}
#[cfg(target_os = "macos")]
fn accessibility_trusted() -> bool {
    tcc::accessibility()
}
#[cfg(not(target_os = "macos"))]
fn accessibility_trusted() -> bool {
    true
}

#[tauri::command]
async fn list_targets(timeout_secs: u64) -> Result<Vec<TargetInfo>, String> {
    let peers = tauri::async_runtime::spawn_blocking(move || {
        browse_targets(Duration::from_secs(timeout_secs))
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(peers
        .into_iter()
        .map(|p| TargetInfo {
            name: p.name,
            addrs: p.addrs.into_iter().map(|a| a.to_string()).collect(),
            port: p.port,
        })
        .collect())
}

#[tauri::command]
fn start_target(
    app: AppHandle,
    state: State<'_, AppState>,
    windowed: bool,
    no_input: bool,
    audio: Option<bool>,
) -> Result<(), String> {
    let audio_playback = audio.unwrap_or(false);
    if state.session.lock().is_some() {
        return Err("a session is already running".into());
    }
    // Sync Tauri commands run on the app MAIN thread — exactly what the
    // AppKit presenter requires. Engine workers drive it via handles.
    // (Linux target role is not implemented yet; run_target reports it.)
    #[cfg(target_os = "macos")]
    let presenter: Option<EmbeddedPresenter> = {
        let app2 = app.clone();
        Some(
            EmbeddedPresenter::new(
                windowed,
                std::sync::Arc::new(move |f: Box<dyn FnOnce() + Send>| {
                    let _ = app2.run_on_main_thread(f);
                }),
            )
            .map_err(|e| format!("create presenter: {e:#}"))?,
        )
    };
    #[cfg(not(target_os = "macos"))]
    let presenter: Option<std::convert::Infallible> = None;
    let cancel = CancelToken::new();
    *state.session.lock() = Some(Session { role: "target", cancel: cancel.clone() });
    emit_state(&app, status_of(&state));

    let app2 = app.clone();
    std::thread::Builder::new()
        .name("tl-target".into())
        .spawn(move || {
            // Announce until the session ends (SPEC §3); non-fatal.
            let announcer = announce_target("thunderlink-target")
                .map_err(|e| log::warn!("mDNS announce failed ({e})"))
                .ok();
            let (sink, rx) = EventSink::channel();
            // Pump events on a dedicated thread; the role needs the thread.
            let app3 = app2.clone();
            std::thread::spawn(move || {
                for ev in rx {
                    let _ = app3.emit("engine://event", &ev);
                }
            });
            let r = run_target(
                TargetConfig {
                    bind: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    windowed,
                    no_input,
                    audio_playback,
                    cancel,
                },
                presenter,
                &sink,
            );
            drop(announcer);
            if let Err(e) = r {
                log::error!("target role ended: {e:#}");
                let _ = app2.emit(
                    "engine://event",
                    &EngineEvent::Ended(format!("error: {e:#}")),
                );
            }
            // ALWAYS cancel so the slot cleaner frees the session slot,
            // even on clean exit or error (fixes 'session in progress').
            if let Some(sess) = app2.state::<AppState>().session.lock().as_ref() {
                sess.cancel.cancel();
            }
            // NOTE: session slot cleared by the outer state watcher below.
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn start_initiator(
    app: AppHandle,
    state: State<'_, AppState>,
    opts: InitiatorOptions,
) -> Result<(), String> {
    if state.session.lock().is_some() {
        return Err("a session is already running".into());
    }
    let addr: SocketAddr = if opts.discover {
        discover_target(Duration::from_secs(10)).map_err(|e| e.to_string())?
    } else {
        let raw = opts.addr.clone().unwrap_or_default();
        if raw.contains(':') {
            raw.parse().map_err(|e| format!("invalid address: {e}"))?
        } else {
            SocketAddr::new(
                raw.parse().map_err(|e| format!("invalid host: {e}"))?,
                CONTROL_PORT,
            )
        }
    };
    let source = match opts.source.as_str() {
        "screen" => Source::Screen,
        _ => Source::TestPattern,
    };
    let codec = match opts.codec.as_deref() {
        Some("h264") => Some(tl_proto::Codec::H264),
        Some(_) => Some(tl_proto::Codec::Hevc),
        None => None,
    };
    let res = opts.res.as_deref().and_then(|r| {
        let (w, h) = r.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });

    let cancel = CancelToken::new();
    *state.session.lock() = Some(Session { role: "initiator", cancel: cancel.clone() });
    emit_state(&app, status_of(&state));

    let app2 = app.clone();
    std::thread::Builder::new()
        .name("tl-initiator".into())
        .spawn(move || {
            let (sink, rx) = EventSink::channel();
            let app3 = app2.clone();
            std::thread::spawn(move || {
                for ev in rx {
                    let _ = app3.emit("engine://event", &ev);
                }
            });
            let r = run_initiator(
                InitiatorConfig {
                    addr,
                    source,
                    codec,
                    bitrate_kbps: opts.bitrate_kbps,
                    fps: opts.fps,
                    res,
                    virtual_display: opts.virtual_display,
                    max_frames: None,
                    audio: match opts.audio.as_deref() {
                        Some("sine") => Some(AudioSource::Sine {
                            freq_hz: opts.audio_freq_hz.unwrap_or(440.0),
                        }),
                        Some("system") => Some(AudioSource::System),
                        _ => None,
                    },
                    cancel,
                },
                &sink,
            );
            if let Err(e) = r {
                log::error!("initiator role ended: {e:#}");
                let _ = app2.emit(
                    "engine://event",
                    &EngineEvent::Ended(format!("error: {e:#}")),
                );
            }
            // ALWAYS cancel so the slot cleaner frees the session slot,
            // even on clean exit or error (fixes 'session in progress').
            if let Some(sess) = app2.state::<AppState>().session.lock().as_ref() {
                sess.cancel.cancel();
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn stop_session(state: State<'_, AppState>) -> Result<(), String> {
    let g = state.session.lock();
    if let Some(s) = g.as_ref() {
        s.cancel.cancel();
    }
    Ok(())
}

/// Best-effort hostname for the window title.
fn hostname_or_default() -> String {
    std::process::Command::new("scutil")
        .arg("--get")
        .arg("LocalHostName")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("HOST"))
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "Unknown".to_string())
}

/// Session-slot watchdog: clears the slot when the cancel token fires OR
/// when the session has been running for >2 s without any engine events
/// (the event pump thread dropped = role thread exited). The previous
/// version only checked cancel.is_cancelled(), which misses error exits.
fn spawn_slot_cleaner(app: AppHandle) {
    std::thread::spawn(move || {
        let mut started = std::time::Instant::now();
        let mut was_running = false;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let state = app.state::<AppState>();
            let g = state.session.lock();
            if let Some(s) = g.as_ref() {
                if !was_running {
                    started = std::time::Instant::now();
                    was_running = true;
                }
                if s.cancel.is_cancelled() {
                    drop(g);
                    *state.session.lock() = None;
                    emit_state(&app, status_of(&state));
                    was_running = false;
                    log::info!("session slot cleared (cancelled)");
                }
            } else {
                if was_running {
                    log::info!("session slot cleared (already empty)");
                    was_running = false;
                }
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            spawn_slot_cleaner(app.handle().clone());

            // Dynamic title: "ThunderLink — <hostname>"
            let hostname = hostname_or_default();
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_title(&format!("ThunderLink — {hostname}"));
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_permissions,
            request_permissions,
            open_permission_pane,
            list_targets,
            start_target,
            start_initiator,
            stop_session
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Open the relevant Privacy & Security pane in System Settings.
/// Trigger the OS consent prompts for any missing permissions (macOS
/// first-run flow; no-op elsewhere). Safe to call repeatedly.
#[tauri::command]
fn request_permissions() -> Result<(), String> {
    tcc::request_missing();
    Ok(())
}

#[tauri::command]
fn open_permission_pane(kind: String) -> Result<(), String> {
    let pane = match kind.as_str() {
        "screen" => "Privacy_ScreenCapture",
        "accessibility" => "Privacy_Accessibility",
        "input" => "Privacy_ListenEvent",
        _ => return Err(format!("unknown permission pane: {kind}")),
    };
    std::process::Command::new("open")
        .arg(format!(
            "x-apple.systempreferences:com.apple.preference.security?{pane}"
        ))
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}
