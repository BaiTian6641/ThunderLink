//! macOS virtual display (private CGVirtualDisplay) + panel info/EDID.
//! SPEC.md §10.

pub mod panel;
pub mod virt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_panel_sane_values() {
        let panel = match panel::main_panel() {
            Ok(p) => p,
            Err(e) => {
                // Headless CI without any window-server session: skip.
                eprintln!("main_panel unavailable, skipping assertions: {e:#}");
                return;
            }
        };
        assert!(panel.width >= 800, "width {} < 800", panel.width);
        eprintln!(
            "main_panel: {}x{} @{:.2}Hz scale_x100={} edid={}",
            panel.width,
            panel.height,
            panel.refresh_millihertz as f64 / 1000.0,
            panel.scale_x100,
            panel.edid.as_ref().map_or("None".into(), |e| format!("{} bytes", e.len()))
        );
        assert!(panel.height >= 600, "height {} < 600", panel.height);
        assert!(
            panel.scale_x100 % 100 == 0 && (100..=400).contains(&panel.scale_x100),
            "scale_x100 {} not a sane multiple of 100",
            panel.scale_x100
        );
        assert!(
            panel.refresh_millihertz >= 24_000,
            "refresh {} mHz below 24 Hz",
            panel.refresh_millihertz
        );
        if let Some(edid) = &panel.edid {
            assert!(edid.len() >= 128, "EDID shorter than base block");
            assert_eq!(edid[0], 0x00, "EDID header byte");
            assert_eq!(edid[1], 0xFF, "EDID header byte");
        }
    }

    #[test]
    fn main_display_points_consistent_with_pixels() {
        let points = match panel::main_display_points() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("main_display_points unavailable, skipping: {e:#}");
                return;
            }
        };
        assert!(points.0 > 0.0 && points.1 > 0.0);
        if let Ok(panel) = panel::main_panel() {
            // Points never exceed native pixels.
            assert!(
                points.0 <= panel.width as f64 + 0.5,
                "points {:?} exceed pixels {}x{}",
                points,
                panel.width,
                panel.height
            );
            assert!(
                points.1 <= panel.height as f64 + 0.5,
                "points {:?} exceed pixels {}x{}",
                points,
                panel.width,
                panel.height
            );
        }
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetOnlineDisplayList(max: u32, ids: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayBounds(display: u32) -> core_graphics::geometry::CGRect;
    }

    fn online_display_ids() -> Vec<u32> {
        let mut ids = [0u32; 64];
        let mut count = 0u32;
        // SAFETY: `ids` has capacity 64; `count` is written on success.
        let err = unsafe { CGGetOnlineDisplayList(64, ids.as_mut_ptr(), &mut count) };
        assert_eq!(err, 0, "CGGetOnlineDisplayList failed: {err}");
        ids[..count as usize].to_vec()
    }

    /// Creating a real virtual display requires a window-server session;
    /// gate behind TL_E2E=1 (SPEC §9). Verifies create -> display_id != 0 ->
    /// Drop removes the display from the online list, plus HiDPI geometry
    /// (point size = half the pixel size).
    #[test]
    fn virtual_display_create_and_drop() {
        if std::env::var_os("TL_E2E").is_none() {
            eprintln!("TL_E2E not set; skipping virtual display lifecycle test");
            return;
        }
        let cfg = virt::VirtualDisplayConfig {
            width: 1280,
            height: 800,
            refresh_millihertz: 60_000,
            hidpi: false,
            name: "ThunderLink E2E".to_string(),
        };
        let display = virt::VirtualDisplay::create(cfg.clone())
            .expect("virtual display creation must succeed under TL_E2E=1");
        let id = display.display_id();
        assert_ne!(id, 0, "display_id must be non-zero");
        eprintln!("virtual display created with display_id={id}");
        assert!(
            online_display_ids().contains(&id),
            "new virtual display {id} not online"
        );

        // HiDPI: pixel mode = width×height, point size half that.
        let retina = virt::VirtualDisplay::create(virt::VirtualDisplayConfig {
            width: 2560,
            height: 1600,
            hidpi: true,
            ..cfg
        })
        .expect("hidpi virtual display creation failed");
        // The window server applies the new display asynchronously.
        std::thread::sleep(std::time::Duration::from_secs(1));
        // SAFETY: `retina` is a live online display id.
        let bounds = unsafe { CGDisplayBounds(retina.display_id()) };
        assert_eq!(
            (bounds.size.width as u32, bounds.size.height as u32),
            (1280, 800),
            "hidpi display must have point size half the pixel size"
        );

        // Drop destroys the displays; windows migrate back to real displays.
        let retina_id = retina.display_id();
        drop(display);
        drop(retina);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ids = online_display_ids();
            if !ids.contains(&id) && !ids.contains(&retina_id) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "dropped virtual displays {id}/{retina_id} still online after 10s: {ids:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        eprintln!("both virtual displays removed after drop");
    }
}
