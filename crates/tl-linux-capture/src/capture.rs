//! X11 root-window screen capture via `GetImage` (ZPixmap, depth 24) —
//! docs/LINUX-PORT.md. One full-screen round trip per call; the Linux
//! initiator v1 runs against X11 (Xorg or Xvfb). Wayland (PipeWire
//! portal ScreenCast) arrives in v2 per the port plan.

use anyhow::{anyhow, bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use super::frame::RawFrame;

/// Captures the X root window (the whole screen) of `$DISPLAY`.
pub struct ScreenCapturer {
    conn: RustConnection,
    root: u32,
    width: u32,
    height: u32,
    fps: u32,
}

impl ScreenCapturer {
    /// Connect to the X server named by `$DISPLAY` at the intended
    /// capture cadence (the caller paces `next_frame`; `fps` is a hint
    /// carried for the engine).
    pub fn new(fps: u32) -> Result<Self> {
        let display = std::env::var("DISPLAY").unwrap_or_default();
        if display.is_empty() {
            bail!(
                "DISPLAY is not set: no X server to capture. Run the initiator \
                 under X11/Xvfb (e.g. DISPLAY=:0) or export DISPLAY"
            );
        }
        Self::connect_to(&display, fps)
    }

    /// Connect to an explicit X display string (e.g. `":0"`, `":90"`).
    fn connect_to(display: &str, fps: u32) -> Result<Self> {
        let (conn, screen_idx): (RustConnection, usize) = x11rb::connect(Some(display))
            .with_context(|| {
                format!(
                    "cannot connect to X display {display:?}: check that $DISPLAY names a \
                     running server (Xorg/Xvfb) and that X access permission lets this user \
                     connect (xauth/xhost)"
                )
            })?;
        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_idx)
            .ok_or_else(|| anyhow!("X server reports no screen {screen_idx}"))?;

        if screen.root_depth != 24 {
            bail!(
                "root window depth is {}, this capturer requires a depth-24 TrueColor root \
                 (start Xvfb with e.g. `-screen 0 1280x720x24`)",
                screen.root_depth
            );
        }
        // ZPixmap byte order handling below assumes the standard 8-bit
        // TrueColor masks; reject exotic visuals instead of mangling colors.
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)
            .ok_or_else(|| anyhow!("root visual {} not listed by the server", screen.root_visual))?;
        if visual.red_mask != 0xFF_0000 || visual.green_mask != 0x00_FF00 || visual.blue_mask != 0x0000_FF
        {
            bail!(
                "unsupported root visual masks r={:#x} g={:#x} b={:#x}; need 8-bit TrueColor",
                visual.red_mask,
                visual.green_mask,
                visual.blue_mask
            );
        }

        let (width, height) = (screen.width_in_pixels as u32, screen.height_in_pixels as u32);
        if width == 0 || height == 0 || width > u16::MAX as u32 || height > u16::MAX as u32 {
            bail!("unusable root window geometry {width}x{height}");
        }
        log::debug!(
            "X11 capturer: display {display:?}, root 0x{:x}, {width}x{height}, depth 24",
            screen.root
        );

        let root = screen.root;
        Ok(Self { conn, root, width, height, fps })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Intended capture cadence (frames per second), as passed to `new`.
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Grab the root window now. One `GetImage` (ZPixmap, all planes)
    /// per call; the reply is converted to tightly packed 32BGRA with
    /// `pts_us` stamped at capture time (`tl_proto::time::now_us`).
    pub fn next_frame(&mut self) -> Result<RawFrame> {
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                self.width as u16,
                self.height as u16,
                u32::MAX,
            )
            .context("X GetImage request failed")?
            .reply()
            .context("X GetImage failed on the root window")?;

        if reply.depth != 24 {
            bail!("GetImage returned depth {}, want 24", reply.depth);
        }
        let expect = self.width as usize * self.height as usize * 4;
        if reply.data.len() != expect {
            bail!(
                "GetImage returned {} bytes, want {expect} ({}x{} 32bpp ZPixmap)",
                reply.data.len(),
                self.width,
                self.height
            );
        }
        let order = self.conn.setup().image_byte_order;
        let bgra = zpixmap_depth24_to_bgra(&reply.data, order);
        Ok(RawFrame {
            width: self.width,
            height: self.height,
            pts_us: tl_proto::time::now_us(),
            bgra,
        })
    }
}

/// Convert a depth-24 ZPixmap reply (32 bits per pixel, pad byte
/// undefined) into tightly packed 32BGRA. A pixel's 24-bit value is
/// `r<<16 | g<<8 | b`, so the in-memory byte order of the server decides
/// where B/G/R land; alpha is forced to 255.
fn zpixmap_depth24_to_bgra(data: &[u8], order: ImageOrder) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        let (b, g, r) = match order {
            ImageOrder::MSB_FIRST => (px[3], px[2], px[1]),
            // LSB_FIRST (the only other defined value; reserved values
            // default to little-endian, as on every real server).
            _ => (px[0], px[1], px[2]),
        };
        out.extend_from_slice(&[b, g, r, 0xFF]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use parking_lot::Mutex;
    use std::time::{Duration, Instant};

    /// Serializes tests that touch the process-wide `DISPLAY` variable.
    static DISPLAY_LOCK: Mutex<()> = parking_lot::const_mutex(());

    #[test]
    fn constructor_error_without_display_mentions_display() {
        let _guard = DISPLAY_LOCK.lock();
        let saved = std::env::var_os("DISPLAY");
        std::env::remove_var("DISPLAY");
        let err = ScreenCapturer::new(60).err().unwrap().to_string();
        if let Some(v) = saved {
            std::env::set_var("DISPLAY", v);
        }
        assert!(err.contains("DISPLAY"), "error must mention DISPLAY: {err}");
    }

    /// Spawn a private Xvfb (1280x720x24) on a free display and return it
    /// together with an already-warm capturer. The probe requires a full
    /// capture round trip, and `-noreset` keeps the server alive when the
    /// probe's connection drops (default Xvfb resets on last-client-exit,
    /// which would race the test's own connect). `TL_E2E=1` gates the
    /// caller (needs an X-capable environment; the container has Xvfb).
    fn spawn_xvfb() -> Option<(Child, String, ScreenCapturer)> {
        for n in 90..100 {
            let display = format!(":{n}");
            let mut child = Command::new("Xvfb")
                .args([
                    display.as_str(),
                    "-screen",
                    "0",
                    "1280x720x24",
                    "-nolisten",
                    "tcp",
                    "-noreset",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Xvfb binary present");
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(Some(_)) = child.try_wait() {
                    break; // display busy: Xvfb exited, try the next one
                }
                // Warm probe: connect AND complete a GetImage round trip.
                let probe = ScreenCapturer::connect_to(&display, 60)
                    .and_then(|mut c| c.next_frame().map(|_| c));
                if let Ok(capturer) = probe {
                    return Some((child, display, capturer));
                }
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        None
    }

    #[test]
    fn e2e_captures_real_1280x720_root_window() {
        if std::env::var("TL_E2E").ok().as_deref() != Some("1") {
            eprintln!("skipping: set TL_E2E=1 to run live X11 capture");
            return;
        }
        let Some((mut xvfb, display, mut capturer)) = spawn_xvfb() else {
            panic!("could not start Xvfb on any display :90..:99");
        };
        assert_eq!((capturer.width(), capturer.height()), (1280, 720));
        assert_eq!(capturer.fps(), 60);

        let f1 = capturer.next_frame().unwrap();
        assert_eq!((f1.width, f1.height), (1280, 720));
        assert_eq!(f1.bgra.len(), 1280 * 720 * 4);
        assert!(f1.pts_us > 0, "wall-clock pts must be stamped");
        // Fresh Xvfb root window is black with an undefined pad byte that
        // we normalize to opaque alpha.
        assert_eq!(&f1.bgra[0..4], &[0, 0, 0, 255]);
        let mid = (360 * 1280 + 640) * 4;
        assert_eq!(&f1.bgra[mid..mid + 4], &[0, 0, 0, 255]);

        let f2 = capturer.next_frame().unwrap();
        assert!(f2.pts_us >= f1.pts_us, "pts must not go backwards");

        // A capturer opened via the env path ($DISPLAY) reaches the same
        // server (constructor covered end-to-end).
        {
            let _guard = DISPLAY_LOCK.lock();
            let saved = std::env::var_os("DISPLAY");
            std::env::set_var("DISPLAY", &display);
            let via_env = ScreenCapturer::new(60).map(|c| (c.width(), c.height()));
            if let Some(v) = saved {
                std::env::set_var("DISPLAY", v);
            } else {
                std::env::remove_var("DISPLAY");
            }
            assert_eq!(via_env.unwrap(), (1280, 720));
        }

        xvfb.kill().unwrap();
        let _ = xvfb.wait();
    }
}
