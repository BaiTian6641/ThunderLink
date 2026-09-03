//! Wayland screen capture via the xdg-desktop-portal ScreenCast API
//! (docs/LINUX-PORT.md): D-Bus drives the portal session, the portal's
//! `OpenPipewireRemote` reply hands us a PipeWire socket fd, and a
//! `pw_stream` delivers RAW BGRA buffers at native resolution (SPEC §1 —
//! no resolution reduction; cursor mode embedded so the pointer is part
//! of the frames). Mirrors the X11 `ScreenCapturer` contract with the
//! same `next_frame` -> [`RawFrame`] shape.
//!
//! D-Bus call sequence (all on `org.freedesktop.portal.Desktop`):
//!
//! ```text
//! CreateSession{handle_token, session_handle_token} -> session path
//! SelectSources{multiple=false, types=1 monitor, cursor_mode=1 embedded}
//! Start{parent_window=""} -> Response streams: [(node path, {size, ..})]
//! OpenPipewireRemote{session} -> fd (h)
//! ```
//!
//! Each request's result arrives as a `Response(code, a{sv})` signal on
//! the request object path derived from the handle token (subscribed
//! before the call to avoid the classic reply/signal race). The consent
//! and source-picker dialogs are shown by the portal itself; `new`
//! blocks until the user answers — that IS the Linux permission flow
//! (SPEC §12.2 spirit; there is no TCC-style preflight to poll).

use anyhow::{bail, Context, Result};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use pipewire as pw;
use pipewire::properties::properties;
use pipewire::spa;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::format::{FormatProperties, MediaType, MediaSubtype};
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::pod::{
    serialize::PodSerializer, ChoiceValue, Object as PodObject, Pod, Property as PodProperty,
    Value as PodValue,
};
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};
use zbus::blocking::proxy::SignalIterator;
use zbus::blocking::{Connection as DbusConnection, Proxy as DbusProxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

use super::frame::RawFrame;

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE_SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";
const IFACE_REQUEST: &str = "org.freedesktop.portal.Request";

/// How long `new` waits for the PipeWire format negotiation to complete
/// once the fd is open (the consent dialog happens earlier, inside the
/// `Start` call, and is unbounded by design).
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `next_frame` waits for a fresher buffer before declaring the
/// stream stalled (the engine paces `next_frame` at `fps`).
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// Wayland screen capturer: one portal ScreenCast session feeding a
/// PipeWire stream, drained latest-wins like the rest of the codebase.
pub struct PortalCapturer {
    shared: Arc<Shared>,
    fps: u32,
    loop_ptr: LoopPtr,
    thread: Option<std::thread::JoinHandle<()>>,
    seen: u64,
}

// SAFETY: the only non-Send field is the raw `pw_main_loop` pointer,
// which is used solely in `Drop` to call `pw_main_loop_quit` — a
// thread-safe PipeWire API — before joining the owning thread.
unsafe impl Send for PortalCapturer {}

/// Raw pointer to the PipeWire main loop owned by the stream thread;
/// only ever used with the thread-safe `pw_main_loop_quit`.
struct LoopPtr(*mut pw::sys::pw_main_loop);

// SAFETY: the pointee stays alive until the stream thread is joined,
// and every use is the thread-safe `pw_main_loop_quit`.
unsafe impl Send for LoopPtr {}

/// State shared with the PipeWire loop thread: the latest-wins frame
/// slot (generation counter so `next_frame` waits for a *newer* frame,
/// same pattern as `tl_video::chan`), the negotiated size, and a
/// terminal error once the stream dies.
#[derive(Default)]
struct State {
    gen: u64,
    latest: Option<RawFrame>,
    dims: Option<(u32, u32)>,
    dead: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

impl Shared {
    /// Publish a frame, replacing any undelivered one (drop-oldest).
    fn publish(&self, frame: RawFrame) {
        let mut st = self.state.lock();
        st.gen = st.gen.wrapping_add(1);
        st.latest = Some(frame);
        self.cv.notify_all();
    }

    /// Record a terminal error and wake every waiter.
    fn die(&self, why: String) {
        let mut st = self.state.lock();
        if st.dead.is_none() {
            st.dead = Some(why);
        }
        self.cv.notify_all();
    }
}

/// User data carried into the `pw_stream` listener callbacks: the
/// negotiated raw format and size (updated on every Format param).
struct StreamUser {
    format: Option<VideoFormat>,
    width: u32,
    height: u32,
}

impl PortalCapturer {
    /// Create the portal session (consent dialog blocks here), open the
    /// PipeWire fd and negotiate a RAW BGRA stream at native size and
    /// the intended `fps` cadence (the caller paces `next_frame`; `fps`
    /// is a hint carried for the engine).
    pub fn new(fps: u32) -> Result<Self> {
        let handoff = open_portal_session()?;
        log::debug!(
            "portal capturer: session started, pipewire node {}, advertised size {:?}",
            handoff.node_id,
            handoff.size
        );

        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
        });
        let (loop_ptr, thread) = spawn_pipewire(handoff, fps, shared.clone())?;

        // Wait for the first format negotiation so width()/height() are
        // valid as soon as the constructor returns (mirrors the X11
        // capturer, which knows the root window geometry up front).
        let deadline = Instant::now() + NEGOTIATE_TIMEOUT;
        {
            let mut st = shared.state.lock();
            while st.dims.is_none() && st.dead.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                shared.cv.wait_for(&mut st, left);
            }
        }
        let (dims, dead) = {
            let st = shared.state.lock();
            (st.dims, st.dead.clone())
        };
        match dims {
            Some((w, h)) => {
                log::debug!("portal capturer: negotiated {w}x{h} raw stream");
            }
            None => {
                teardown(&loop_ptr, Some(thread));
                bail!(
                    "the portal PipeWire stream did not negotiate a video format within {}s: {}",
                    NEGOTIATE_TIMEOUT.as_secs(),
                    dead.unwrap_or_else(|| "no format arrived".to_owned())
                );
            }
        }

        Ok(Self { shared, fps, loop_ptr, thread: Some(thread), seen: 0 })
    }

    /// Negotiated stream width in pixels.
    pub fn width(&self) -> u32 {
        self.shared.state.lock().dims.map(|d| d.0).unwrap_or(0)
    }

    /// Negotiated stream height in pixels.
    pub fn height(&self) -> u32 {
        self.shared.state.lock().dims.map(|d| d.1).unwrap_or(0)
    }

    /// Intended capture cadence (frames per second), as passed to `new`.
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Take the newest frame, waiting for one newer than anything
    /// previously returned. `pts_us` is wall-clock, stamped at the
    /// source (`tl_proto::time::now_us`).
    pub fn next_frame(&mut self) -> Result<RawFrame> {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        let mut st = self.shared.state.lock();
        loop {
            if st.gen != self.seen && st.latest.is_some() {
                self.seen = st.gen;
                return Ok(st.latest.take().expect("checked is_some"));
            }
            if let Some(err) = &st.dead {
                bail!("the portal screen cast stream ended: {err}");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "no frame arrived from the portal stream within {}s \
                     (compositor or portal stalled?)",
                    FRAME_TIMEOUT.as_secs()
                );
            }
            self.shared.cv.wait_for(&mut st, left);
        }
    }
}

impl Drop for PortalCapturer {
    fn drop(&mut self) {
        teardown(&self.loop_ptr, self.thread.take());
    }
}

fn teardown(loop_ptr: &LoopPtr, thread: Option<std::thread::JoinHandle<()>>) {
    // SAFETY: `pw_main_loop_quit` is documented thread-safe; the loop
    // object stays alive on its thread until `run` returns, and we join
    // that thread below before returning.
    unsafe { pw::sys::pw_main_loop_quit(loop_ptr.0) };
    if let Some(t) = thread {
        let _ = t.join();
    }
}

// ---------------------------------------------------------------------------
// Portal D-Bus handshake
// ---------------------------------------------------------------------------

struct PortalHandoff {
    /// fd returned by `OpenPipewireRemote` — a PipeWire socket.
    fd: OwnedFd,
    /// PipeWire global object id of the chosen stream (from `Start`).
    node_id: u32,
    /// Advertised stream size from the `Start` response, if present.
    size: Option<(u32, u32)>,
}

fn open_portal_session() -> Result<PortalHandoff> {
    let conn = DbusConnection::session().with_context(|| {
        "no D-Bus session bus available to reach the xdg-desktop-portal: Wayland screen \
         capture needs a running desktop session. Check that a session bus exists \
         (DBUS_SESSION_BUS_ADDRESS) and the xdg-desktop-portal service is running"
    })?;
    let screencast = DbusProxy::new(&conn, PORTAL_DEST, PORTAL_PATH, IFACE_SCREENCAST)
        .context("cannot build the ScreenCast D-Bus proxy")?;
    // cursor_mode (embedded) needs interface version >= 4 (portal 1.10+).
    let version: u32 = screencast.get_property("version").unwrap_or(0);
    let sender = sanitized_sender(&conn)?;
    let pid = std::process::id();
    let mut n = 0u32;
    let mut token = || {
        n += 1;
        format!("tl{pid}_{n}")
    };

    // --- CreateSession -------------------------------------------------
    let tok = token();
    let mut options: HashMap<&str, ZValue> = HashMap::new();
    options.insert("handle_token", ZValue::from(tok.as_str()));
    let session_token = format!("{tok}s");
    options.insert("session_handle_token", ZValue::from(session_token.as_str()));
    let session = create_session(&conn, &screencast, &sender, &tok, &options)?;
    log::debug!("portal session: {}", session.as_str());
    // --- SelectSources -------------------------------------------------
    let tok = token();
    let mut options: HashMap<&str, ZValue> = HashMap::new();
    options.insert("handle_token", ZValue::from(tok.as_str()));
    // types: 1 = MONITOR (whole desktop); multiple: single stream;
    // native resolution is whatever the source advertises (SPEC §1 —
    // no ReduceResolution option is passed).
    options.insert("types", ZValue::from(1u32));
    options.insert("multiple", ZValue::from(false));
    if version >= 4 {
        // cursor_mode: 1 = EMBEDDED (pointer burned into the frames).
        options.insert("cursor_mode", ZValue::from(1u32));
    }
    portal_request(&conn, &screencast, &sender, &tok, |sc| {
        sc.call_method("SelectSources", &(&session, &options))
            .context("ScreenCast.SelectSources failed")?;
        Ok(())
    })?;

    // --- Start (this is where the consent dialog blocks) ---------------
    let tok = token();
    let mut options: HashMap<&str, ZValue> = HashMap::new();
    options.insert("handle_token", ZValue::from(tok.as_str()));
    let results = portal_request(&conn, &screencast, &sender, &tok, |sc| {
        sc.call_method("Start", &(&session, "", &options))
            .context("ScreenCast.Start failed")?;
        Ok(())
    })?;
    let (node_id, size) = parse_streams(&results)?;

    // --- OpenPipewireRemote --------------------------------------------
    let options: HashMap<&str, ZValue> = HashMap::new();
    let fd: zbus::zvariant::OwnedFd = screencast
        .call_method("OpenPipewireRemote", &(&session, &options))
        .context("ScreenCast.OpenPipewireRemote failed")?
        .body()
        .deserialize()
        .context("malformed OpenPipewireRemote reply")?;
    let fd = fd.as_fd().try_clone_to_owned().context("dup portal fd")?;

    Ok(PortalHandoff { fd, node_id, size })
}

/// Run one portal request end-to-end: subscribe to
/// `org.freedesktop.portal.Request::Response` on the request path
/// predicted from the handle token (before the call, so the
/// reply/signal race cannot bite), perform the method call, then block
/// for the response. The proxy and its path stay function-local —
/// zbus proxies borrow their object path.
fn portal_request<F>(
    conn: &DbusConnection,
    screencast: &DbusProxy<'_>,
    sender: &str,
    token: &str,
    invoke: F,
) -> Result<HashMap<String, OwnedValue>>
where
    F: FnOnce(&DbusProxy<'_>) -> Result<()>,
{
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let proxy = DbusProxy::new(conn, PORTAL_DEST, path.as_str(), IFACE_REQUEST)
        .with_context(|| format!("cannot create Request proxy at {path}"))?;
    let mut signals = proxy
        .receive_signal("Response")
        .context("cannot subscribe to the portal Response signal")?;
    invoke(screencast)?;
    next_response(&mut signals)
}

/// `CreateSession` is the one request whose result may arrive in the
/// method reply itself (portal >= 1.10) instead of a Response signal
/// (older portals), so it subscribes before the single call and then
/// decides from the reply shape.
fn create_session(
    conn: &DbusConnection,
    screencast: &DbusProxy<'_>,
    sender: &str,
    token: &str,
    options: &HashMap<&str, ZValue<'_>>,
) -> Result<OwnedObjectPath> {
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let proxy = DbusProxy::new(conn, PORTAL_DEST, path.as_str(), IFACE_REQUEST)
        .with_context(|| format!("cannot create Request proxy at {path}"))?;
    let mut signals = proxy
        .receive_signal("Response")
        .context("cannot subscribe to the portal Response signal")?;
    let reply: OwnedObjectPath = screencast
        .call_method("CreateSession", options)
        .context("ScreenCast.CreateSession failed")?
        .body()
        .deserialize()
        .context("malformed CreateSession reply")?;
    if reply.as_str().contains("/session/") {
        return Ok(reply);
    }
    // Old-style portal: the reply was a request path, the real session
    // handle arrives in the Response results.
    let results = next_response(&mut signals)?;
    let handle = results
        .get("session_handle")
        .context("old-style portal Response missing session_handle")?;
    OwnedObjectPath::try_from(owned_value_as_string(handle)?)
        .context("session_handle is not an object path")
}

/// Block for the next `Response` signal. There is deliberately no
/// timeout: the user may sit on the consent/picker dialog for as long
/// as they like (same contract as the macOS TCC prompt).
fn next_response(
    signals: &mut SignalIterator<'_>,
) -> Result<HashMap<String, OwnedValue>> {
    let msg = signals
        .next()
        .context("the portal vanished before answering (no Response signal)")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg
        .body()
        .deserialize()
        .context("malformed portal Response signal")?;
    match code {
        0 => Ok(results),
        1 => bail!(
            "screen sharing was denied: the user dismissed or rejected the portal \
             consent dialog"
        ),
        c => bail!("portal request failed with response code {c}"),
    }
}

/// The caller's unique bus name, sanitized the way the portal does when
/// building request paths: drop the leading ':' and map '.' to '_'.
fn sanitized_sender(conn: &DbusConnection) -> Result<String> {
    conn.unique_name()
        .map(|n| n.as_str().trim_start_matches(':').replace('.', "_"))
        .context("the session bus assigned no unique name")
}

fn owned_value_as_string(v: &OwnedValue) -> Result<String> {
    match ZValue::from(v.clone()) {
        ZValue::ObjectPath(p) => Ok(p.to_string()),
        ZValue::Str(s) => Ok(s.to_string()),
        other => bail!("expected an object path, got {:?}", other.value_signature()),
    }
}

/// Extract the first (only) stream from the `Start` response: its
/// PipeWire node id and advertised size, if the portal sent one.
fn parse_streams(results: &HashMap<String, OwnedValue>) -> Result<(u32, Option<(u32, u32)>)> {
    let streams = results
        .get("streams")
        .context("portal Start response carries no streams")?;
    let ZValue::Array(arr) = ZValue::from(streams.clone()) else {
        bail!("portal 'streams' result is not an array");
    };
    let first = arr.iter().next().context("portal stream list is empty")?;
    let ZValue::Structure(s) = first else {
        bail!("portal stream entry is not a struct");
    };
    let fields = s.fields();
    if fields.len() != 2 {
        bail!("portal stream struct has {} fields, want 2", fields.len());
    }
    // The xdg-desktop-portal spec returns the PipeWire node ID as a
    // plain u32 in the first struct field. Some implementations also
    // return it as an object path or string with the ID embedded.
    let node_id = match &fields[0] {
        ZValue::U32(id) => *id,
        ZValue::ObjectPath(p) => node_id_from_path(p.as_str())
            .context("portal node path carries no numeric id")?,
        ZValue::Str(s) => node_id_from_path(s.as_str())
            .context("portal node string carries no numeric id")?,
        other => bail!("portal node id has unexpected type: {:?}", other.value_signature()),
    };
    let props: HashMap<String, OwnedValue> = fields[1]
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("portal stream props are not a{{sv}}: {e}"))?;
    let size = props.get("size").and_then(|v| match ZValue::from(v.clone()) {
        ZValue::Structure(sz) if sz.fields().len() == 2 => {
            let w: i32 = sz.fields()[0].clone().try_into().unwrap_or(0);
            let h: i32 = sz.fields()[1].clone().try_into().unwrap_or(0);
            Some((w.max(0) as u32, h.max(0) as u32))
        }
        _ => None,
    });
    Ok((node_id, size))
}

/// Pull the PipeWire global id out of a portal node path such as
/// `/pipewire:0:3/65` or `/65` — the trailing numeric segment.
fn node_id_from_path(path: &str) -> Option<u32> {
    path.rsplit('/')
        .find(|seg| !seg.is_empty())
        .and_then(|seg| seg.parse::<u32>().ok())
}

// ---------------------------------------------------------------------------
// PipeWire stream
// ---------------------------------------------------------------------------

enum Startup {
    Started(LoopPtr),
    Failed(String),
}

fn spawn_pipewire(
    handoff: PortalHandoff,
    fps: u32,
    shared: Arc<Shared>,
) -> Result<(LoopPtr, std::thread::JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel::<Startup>();
    let thread_shared = shared.clone();
    let thread = std::thread::Builder::new()
        .name("tl-portal-pw".to_owned())
        .spawn(move || {
            let shared = thread_shared.clone();
            pw_thread_main(handoff, fps, thread_shared, tx);
            shared.die("PipeWire main loop exited".to_owned());
        })
        .context("cannot spawn the PipeWire loop thread")?;

    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Startup::Started(ptr)) => Ok((ptr, thread)),
        Ok(Startup::Failed(err)) => {
            let _ = thread.join();
            bail!("cannot start the portal PipeWire stream: {err}");
        }
        Err(_) => {
            shared.die("PipeWire loop thread did not start".to_owned());
            bail!("the PipeWire loop thread did not signal readiness within 10s");
        }
    }
}

fn pw_thread_main(
    handoff: PortalHandoff,
    fps: u32,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Startup>,
) {
    let fail = |what: &str, err: &dyn std::fmt::Display| {
        let _ = tx.send(Startup::Failed(format!("{what}: {err}")));
    };
    let main_loop = match pw::main_loop::MainLoop::new(None) {
        Ok(l) => l,
        Err(e) => return fail("pw_main_loop_new", &e),
    };
    let context = match pw::context::Context::new(&main_loop) {
        Ok(c) => c,
        Err(e) => return fail("pw_context_new", &e),
    };
    // The portal fd is a PipeWire socket: connect the core over it.
    let core = match context.connect_fd(handoff.fd, None) {
        Ok(c) => c,
        Err(e) => return fail("pw_context_connect_fd (portal PipeWire socket)", &e),
    };
    let _ = tx.send(Startup::Started(LoopPtr(main_loop.as_raw_ptr())));

    let stream = match pw::stream::Stream::new(
        &core,
        "ThunderLink",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(e) => return shared.die(format!("pw_stream_new: {e}")),
    };

    let (hint_w, hint_h) = handoff.size.unwrap_or((0, 0));
    let user = StreamUser {
        format: None,
        width: hint_w,
        height: hint_h,
    };

    let state_shared = shared.clone();
    let param_shared = shared.clone();
    let proc_shared = shared.clone();

    let listener = stream
        .add_local_listener_with_user_data(user)
        .state_changed(move |_, _, _old, new| {
            if let pw::stream::StreamState::Error(err) = &new {
                state_shared.die(format!("PipeWire stream error: {err}"));
            } else {
                log::debug!("portal pw stream state: {new:?}");
            }
        })
        .param_changed(move |stream, user, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = param else { return };
            let Ok((mt, ms)) = spa::param::format_utils::parse_format(pod) else {
                return;
            };
            if mt != MediaType::Video || ms != MediaSubtype::Raw {
                return;
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(pod).is_err() {
                return;
            }
            let (w, h) = (info.size().width, info.size().height);
            if w > 0 && h > 0 && w <= u16::MAX as u32 && h <= u16::MAX as u32 {
                user.width = w;
                user.height = h;
            }
            user.format = Some(info.format());
            log::debug!(
                "portal pw format: {:?} {}x{}",
                info.format(),
                user.width,
                user.height
            );
            // Answer with the buffer layout for the negotiated format:
            // one block, stride = width*4, CPU-mappable memory only
            // (MemPtr/MemFd) so `data.data()` is a plain slice. DmaBuf
            // is still handled defensively in `process`.
            let bytes = build_buffers_pod(user.width, user.height);
            let Some(pod) = Pod::from_bytes(&bytes) else {
                param_shared.die("serialized buffer params pod is invalid".to_owned());
                return;
            };
            let mut params = [pod];
            if let Err(e) = stream.update_params(&mut params) {
                log::warn!("portal pw update_params failed: {e}");
            }
            let mut st = param_shared.state.lock();
            if st.dims.is_none() {
                st.dims = Some((user.width, user.height));
            }
            param_shared.cv.notify_all();
        })
        .process(move |stream, user| {
            let (width, height) = (user.width, user.height);
            let Some(format) = user.format else { return };
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let chunk = data.chunk();
            let stride = if chunk.stride() > 0 {
                chunk.stride() as usize
            } else {
                width as usize * 4
            };
            let offset = chunk.offset() as usize;
            let pts = tl_proto::time::now_us();
            let copied: Result<Vec<u8>> = match data.type_() {
                DataType::MemPtr | DataType::MemFd => data
                    .data()
                    .ok_or_else(|| anyhow::anyhow!("mappable buffer has a null data pointer"))
                    .and_then(|mem| {
                        copy_frame_to_bgra(mem, offset, stride, width, height, format)
                    }),
                DataType::DmaBuf => {
                    // We asked for MemPtr/MemFd, but honor a peer that
                    // insists on DmaBuf with a linear layout: map the
                    // fd read-only for the duration of the copy.
                    map_dmabuf_and_copy(data, offset, stride, width, height, format)
                }
                other => Err(anyhow::anyhow!(
                    "unsupported PipeWire buffer data type {other:?}"
                )),
            };
            match copied {
                Ok(bgra) => match RawFrame::new(width, height, pts, bgra) {
                    Ok(frame) => proc_shared.publish(frame),
                    Err(e) => {
                        proc_shared.die(format!("invalid frame from the portal stream: {e}"))
                    }
                },
                Err(e) => {
                    // Unconvertible pixel formats are fatal; transient
                    // copy errors (short buffer on one frame) are not.
                    if !is_supported_format(format) {
                        proc_shared
                            .die(format!("negotiated pixel format {format:?} is not convertible"));
                    } else {
                        log::warn!("portal frame copy failed: {e}");
                    }
                }
            }
        })
        .register();

    if let Err(e) = listener {
        shared.die(format!("cannot register the pw_stream listener: {e}"));
        return;
    }

    let enum_bytes = build_enum_format_pod(fps, hint_w.max(1), hint_h.max(1));
    let Some(pod) = Pod::from_bytes(&enum_bytes) else {
        shared.die("serialized enum-format pod is invalid".to_owned());
        return;
    };
    let mut params = [pod];
    if let Err(e) = stream.connect(
        spa::utils::Direction::Input,
        Some(handoff.node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ) {
        shared.die(format!(
            "pw_stream_connect to node {}: {e}",
            handoff.node_id
        ));
        return;
    }

    main_loop.run();
}

/// The RAW 32-bit RGB byte orders [`copy_frame_to_bgra`] understands.
fn is_supported_format(format: VideoFormat) -> bool {
    format == VideoFormat::BGRA
        || format == VideoFormat::BGRx
        || format == VideoFormat::xBGR
        || format == VideoFormat::RGBA
        || format == VideoFormat::RGBx
        || format == VideoFormat::xRGB
}

/// Build the `SPA_TYPE_Object_Format / SPA_PARAM_EnumFormat` pod offered
/// to `pw_stream_connect`: raw video, any of the 32-bit RGB byte orders
/// (BGRA preferred), any size up to 16384 (the portal/producer decides —
/// native resolution per SPEC §1) and the target framerate.
fn build_enum_format_pod(fps: u32, hint_w: u32, hint_h: u32) -> Vec<u8> {
    let obj = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            FormatProperties::MediaType,
            Id,
            MediaType::Video
        ),
        spa::pod::property!(
            FormatProperties::MediaSubtype,
            Id,
            MediaSubtype::Raw
        ),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRA,
            VideoFormat::BGRx,
            VideoFormat::xBGR,
            VideoFormat::RGBA,
            VideoFormat::RGBx,
            VideoFormat::xRGB
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: hint_w, height: hint_h },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 16384, height: 16384 }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 1000, denom: 1 }
        ),
    );
    serialize_pod(&obj)
}

/// Build the `SPA_PARAM_Buffers` answer for a negotiated raw format:
/// 2..32 buffers, 1 block, `stride*height` bytes, 16-byte alignment,
/// CPU-mappable data types only (MemPtr|MemFd).
fn build_buffers_pod(width: u32, height: u32) -> Vec<u8> {
    let stride = width as i32 * 4;
    let size = stride * height as i32;
    let memptr = 1 << DataType::MemPtr.as_raw();
    let memfd = 1 << DataType::MemFd.as_raw();
    let obj = PodObject {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            PodProperty::new(
                libspa::sys::SPA_PARAM_BUFFERS_buffers,
                PodValue::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range { default: 4, min: 2, max: 32 },
                ))),
            ),
            PodProperty::new(libspa::sys::SPA_PARAM_BUFFERS_blocks, PodValue::Int(1)),
            PodProperty::new(libspa::sys::SPA_PARAM_BUFFERS_size, PodValue::Int(size)),
            PodProperty::new(libspa::sys::SPA_PARAM_BUFFERS_stride, PodValue::Int(stride)),
            PodProperty::new(libspa::sys::SPA_PARAM_BUFFERS_align, PodValue::Int(16)),
            PodProperty::new(
                libspa::sys::SPA_PARAM_BUFFERS_dataType,
                PodValue::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags { default: memptr, flags: vec![memptr, memfd] },
                ))),
            ),
        ],
    };
    serialize_pod(&obj)
}

fn serialize_pod(obj: &PodObject) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &PodValue::Object(obj.clone()))
        .expect("serializing an in-memory pod cannot fail")
        .0
        .into_inner()
}

/// Copy one frame out of a PipeWire buffer into tightly packed 32BGRA.
/// `src` is the whole data block; `offset`/`stride` come from the chunk.
/// Byte orders (memory order per pixel) handled:
///
/// | format | bytes   | output      |
/// |--------|---------|-------------|
/// | BGRA   | B G R A | passthrough |
/// | BGRx   | B G R x | B G R 255   |
/// | xBGR   | x B G R | B G R 255   |
/// | RGBA   | R G B A | swapped     |
/// | RGBx   | R G B x | swapped     |
/// | xRGB   | x R G B | swapped     |
///
/// Anything else is an error.
fn copy_frame_to_bgra(
    src: &[u8],
    offset: usize,
    stride: usize,
    width: u32,
    height: u32,
    format: VideoFormat,
) -> Result<Vec<u8>> {
    if width == 0 || height == 0 {
        bail!("refusing to copy a {width}x{height} frame");
    }
    // (b, g, r, a) byte indices within each 4-byte pixel.
    let (bi, gi, ri, ai) = if format == VideoFormat::BGRA {
        (0usize, 1, 2, Some(3usize))
    } else if format == VideoFormat::BGRx {
        (0, 1, 2, None)
    } else if format == VideoFormat::xBGR {
        (1, 2, 3, None)
    } else if format == VideoFormat::RGBA {
        (2, 1, 0, Some(3))
    } else if format == VideoFormat::RGBx {
        (2, 1, 0, None)
    } else if format == VideoFormat::xRGB {
        (3, 2, 1, None)
    } else {
        bail!("unsupported negotiated pixel format {format:?}");
    };
    let row = width as usize * 4;
    if stride < row {
        bail!("chunk stride {stride} < tight row size {row}");
    }
    let need = offset + stride * (height as usize - 1) + row;
    if need > src.len() {
        bail!("buffer too small: need {need} bytes, have {}", src.len());
    }

    let mut out = Vec::with_capacity(row * height as usize);
    if (bi, gi, ri, ai) == (0, 1, 2, Some(3)) {
        // Tight BGRA fast path: bulk row copies.
        for y in 0..height as usize {
            let s = offset + y * stride;
            out.extend_from_slice(&src[s..s + row]);
        }
    } else {
        for y in 0..height as usize {
            let s = offset + y * stride;
            for px in src[s..s + row].chunks_exact(4) {
                out.push(px[bi]);
                out.push(px[gi]);
                out.push(px[ri]);
                out.push(ai.map_or(0xFF, |i| px[i]));
            }
        }
    }
    Ok(out)
}

/// Map a DmaBuf-backed `spa_data` read-only and copy the frame out of
/// it. Only valid for linear layouts — which is what portal ScreenCast
/// BGRA streams use when they fall back to dmabuf — and reached only
/// when the peer ignored our MemPtr/MemFd preference.
fn map_dmabuf_and_copy(
    data: &mut pipewire::spa::buffer::Data,
    offset: usize,
    stride: usize,
    width: u32,
    height: u32,
    format: VideoFormat,
) -> Result<Vec<u8>> {
    let fd = data.as_raw().fd;
    if fd < 0 {
        bail!("dmabuf data block carries no fd");
    }
    let len = data.as_raw().maxsize as usize;
    if len == 0 {
        bail!("dmabuf data block has zero maxsize");
    }
    let map_len = (len + 0xFFF) & !0xFFF;
    // SAFETY: the raw fd is borrowed for the duration of this mmap from
    // the dequeued pw buffer (valid until the buffer is requeued on
    // return); read-only MAP_SHARED is the dma-buf mapping contract.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd as i32) };
    // SAFETY: page-aligned length, released with munmap below before
    // the buffer is requeued on return.
    let ptr = unsafe {
        mmap(
            None,
            map_len.try_into()?,
            ProtFlags::PROT_READ,
            MapFlags::MAP_SHARED,
            borrowed,
            0,
        )
    }
    .context("cannot mmap the dmabuf buffer")?;
    // SAFETY: `ptr..ptr+map_len` was just mapped; the copy only reads
    // within it and its bounds are checked inside.
    let mem = unsafe { std::slice::from_raw_parts(ptr.as_ptr() as *const u8, map_len) };
    let res = copy_frame_to_bgra(mem, offset, stride, width, height, format);
    // SAFETY: unmap exactly what mmap returned, with the same length.
    unsafe { munmap(ptr, map_len) }.context("munmap of the dmabuf failed")?;
    res
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// Serializes tests that mutate process-wide environment variables.
    static ENV_LOCK: Mutex<()> = parking_lot::const_mutex(());

    #[test]
    fn node_id_from_path_parses_trailing_segment() {
        assert_eq!(node_id_from_path("/65"), Some(65));
        assert_eq!(node_id_from_path("/pipewire:0:3/65"), Some(65));
        assert_eq!(node_id_from_path("pipewire:0:3/91"), Some(91));
    }

    #[test]
    fn node_id_from_path_rejects_non_numeric() {
        assert_eq!(node_id_from_path("/pipewire:0:3"), None);
        assert_eq!(node_id_from_path("/"), None);
        assert_eq!(node_id_from_path(""), None);
    }

    #[test]
    fn copy_bgra_passthrough_with_stride() {
        // 2x2 with a 12-byte stride: 8 data bytes + one pad pixel per row.
        let mut src = vec![0u8; 24];
        src[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src[8..12].copy_from_slice(&[0xEE; 4]); // row-0 pad
        src[12..20].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
        // row-1 pad (20..24) stays zero
        let out = copy_frame_to_bgra(&src, 0, 12, 2, 2, VideoFormat::BGRA).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn copy_bgrx_forces_opaque_alpha() {
        let src: Vec<u8> = (0u8..16).collect(); // 2x2 BGRx
        let out = copy_frame_to_bgra(&src, 0, 8, 2, 2, VideoFormat::BGRx).unwrap();
        assert_eq!(
            out,
            vec![0, 1, 2, 255, 4, 5, 6, 255, 8, 9, 10, 255, 12, 13, 14, 255]
        );
    }

    #[test]
    fn copy_xbgr_reorders() {
        // xBGR: bytes [x, B, G, R]
        let src = vec![9u8, 10, 11, 12];
        let out = copy_frame_to_bgra(&src, 0, 4, 1, 1, VideoFormat::xBGR).unwrap();
        assert_eq!(out, vec![10, 11, 12, 255]);
    }

    #[test]
    fn copy_rgba_swaps_channels_and_keeps_alpha() {
        let src = vec![1u8, 2, 3, 200]; // R,G,B,A
        let out = copy_frame_to_bgra(&src, 0, 4, 1, 1, VideoFormat::RGBA).unwrap();
        assert_eq!(out, vec![3, 2, 1, 200]);
    }

    #[test]
    fn copy_rgbx_and_xrgb_swap_channels() {
        let src = vec![1u8, 2, 3, 0, 4, 5, 6, 0]; // RGBx x2
        let out = copy_frame_to_bgra(&src, 0, 8, 2, 1, VideoFormat::RGBx).unwrap();
        assert_eq!(out, vec![3, 2, 1, 255, 6, 5, 4, 255]);

        let src = vec![0u8, 1, 2, 3, 0, 4, 5, 6]; // xRGB x2: [x,R,G,B]
        let out = copy_frame_to_bgra(&src, 0, 8, 2, 1, VideoFormat::xRGB).unwrap();
        assert_eq!(out, vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn copy_honors_chunk_offset() {
        let mut src = vec![0u8; 12];
        src[4..8].copy_from_slice(&[10, 20, 30, 40]);
        let out = copy_frame_to_bgra(&src, 4, 8, 1, 1, VideoFormat::BGRA).unwrap();
        assert_eq!(out, vec![10, 20, 30, 40]);
    }

    #[test]
    fn copy_rejects_undersized_buffer() {
        let err = copy_frame_to_bgra(&[0u8; 7], 0, 8, 2, 1, VideoFormat::BGRA)
            .unwrap_err()
            .to_string();
        assert!(err.contains("too small"), "{err}");
        let err = copy_frame_to_bgra(&[0u8; 8], 0, 2, 2, 1, VideoFormat::BGRA)
            .unwrap_err()
            .to_string();
        assert!(err.contains("stride"), "{err}");
    }

    #[test]
    fn copy_rejects_unsupported_format() {
        let err = copy_frame_to_bgra(&[0u8; 4], 0, 4, 1, 1, VideoFormat::I420)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported"), "{err}");
    }

    #[test]
    fn enum_format_pod_is_a_valid_object_pod() {
        let bytes = build_enum_format_pod(60, 1920, 1080);
        let pod = Pod::from_bytes(&bytes).expect("serialized pod parses");
        assert_eq!(pod.type_(), SpaTypes::Object);
        assert!(!bytes.is_empty());
    }

    #[test]
    fn buffers_pod_is_a_valid_object_pod() {
        let bytes = build_buffers_pod(1280, 720);
        let pod = Pod::from_bytes(&bytes).expect("serialized pod parses");
        assert_eq!(pod.type_(), SpaTypes::Object);
        // size = stride * height = 1280*4*720
        let needle = (1280i32 * 4 * 720).to_ne_bytes();
        assert!(bytes.windows(4).any(|w| w == needle), "size field missing");
    }

    /// With no session bus (both env-based addresses removed) the
    /// constructor must fail with an error pointing at the portal/D-Bus.
    #[test]
    fn constructor_error_without_dbus_mentions_portal() {
        let _guard = ENV_LOCK.lock();
        let saved_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
        let saved_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
        std::env::remove_var("XDG_RUNTIME_DIR");
        let err = PortalCapturer::new(60).err().unwrap().to_string();
        if let Some(v) = saved_bus {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", v);
        }
        if let Some(v) = saved_runtime {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        }
        let lower = err.to_lowercase();
        assert!(
            lower.contains("portal") || lower.contains("dbus"),
            "error must mention the portal or D-Bus: {err}"
        );
    }

    /// Live round trip against a real Wayland session. NOT runnable in
    /// the build container (no compositor there): run on a desktop with
    /// a compositor + `xdg-desktop-portal(-wlr|gtk|kde)` + PipeWire and
    /// answer the consent dialog when it appears.
    ///   TL_E2E=1 cargo test -p tl-linux-capture portal -- --nocapture
    #[test]
    fn e2e_live_portal_roundtrip() {
        if std::env::var("TL_E2E").ok().as_deref() != Some("1") {
            eprintln!("skipping: set TL_E2E=1 (needs a compositor + running portal)");
            return;
        }
        let mut capturer = PortalCapturer::new(30).unwrap();
        assert!(capturer.width() > 0 && capturer.height() > 0);
        assert_eq!(capturer.fps(), 30);

        let f1 = capturer.next_frame().unwrap();
        assert_eq!((f1.width, f1.height), (capturer.width(), capturer.height()));
        assert_eq!(f1.bgra.len(), f1.width as usize * f1.height as usize * 4);
        assert!(f1.pts_us > 0, "wall-clock pts must be stamped");
        assert_eq!(f1.bgra[3], 0xFF, "alpha must be normalized to opaque");

        let f2 = capturer.next_frame().unwrap();
        assert!(f2.pts_us >= f1.pts_us, "pts must not go backwards");
    }
}
