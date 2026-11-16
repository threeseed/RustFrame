//! X11 region capture smoke test. Runs only when a DISPLAY is available
//! (e.g. `xvfb-run -s "-screen 0 800x600x24" cargo test --test linux_capture`).
#![cfg(target_os = "linux")]

use rustframe_capture::{CaptureEngine, CaptureRect, LinuxCaptureEngine};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

fn wait_frame(eng: &mut LinuxCaptureEngine) -> Option<rustframe_capture::CaptureFrame> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        if eng.has_new_frame() {
            if let Some(f) = eng.get_frame() {
                return Some(f);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

#[test]
fn captures_region_and_idles_when_static() {
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("skipping: no DISPLAY");
        return;
    }
    let (conn, n) = x11rb::connect(None).unwrap();
    let screen = &conn.setup().roots[n];
    let win = conn.generate_id().unwrap();
    // Pure red window at (50,40) 200x100, override-redirect so no WM moves it.
    conn.create_window(24, win, screen.root, 50, 40, 200, 100, 0, WindowClass::INPUT_OUTPUT, 0,
        &CreateWindowAux::new().background_pixel(0x00FF0000).override_redirect(1)).unwrap();
    conn.map_window(win).unwrap();
    conn.sync().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let mut eng = LinuxCaptureEngine::new().unwrap();
    eng.start(CaptureRect::new(60, 50, 64, 32), false, None).unwrap();
    let f = wait_frame(&mut eng).expect("first frame");
    assert_eq!((f.width, f.height, f.stride), (64, 32, 256));
    assert_eq!(&f.data[0..4], &[0, 0, 255, 255], "BGRA red");

    // Static screen: XDamage should suppress further frames.
    std::thread::sleep(Duration::from_millis(300));
    assert!(!eng.has_new_frame(), "no new frame expected for static content");

    // Region update produces a new frame immediately, clipped region partly outside window.
    eng.update_region(CaptureRect::new(0, 0, 100, 100)).unwrap();
    let f = wait_frame(&mut eng).expect("frame after region change");
    assert_eq!((f.width, f.height), (100, 100));
    assert_eq!(&f.data[(60 * 100 + 60) * 4..][..4], &[0, 0, 255, 255]);

    // Damage inside the region triggers a frame.
    conn.change_window_attributes(win, &ChangeWindowAttributesAux::new().background_pixel(0x0000FF00)).unwrap();
    conn.clear_area(false, win, 0, 0, 0, 0).unwrap();
    conn.sync().unwrap();
    let f = wait_frame(&mut eng).expect("frame after damage");
    assert_eq!(&f.data[(60 * 100 + 60) * 4..][..4], &[0, 255, 0, 255]);
    eng.stop();
}
