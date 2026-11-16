// capture/linux.rs - Linux Screen Region Capture (X11)
//
// Captures an arbitrary rectangle of the X11 root window on a background thread.
//
// Performance / compositing techniques used:
// - Region-only grabs: only the requested rectangle is read back, never the
//   whole root window, so bandwidth scales with the shared area.
// - MIT-SHM (memfd + ShmAttachFd): the X server writes pixels straight into a
//   shared mapping; no image data goes over the X socket. Falls back to plain
//   GetImage when SHM is unavailable (e.g. remote X / SSH forwarding).
// - XDamage-driven capture: the thread only grabs when the server reports
//   damage that intersects the region (or the cursor moved / region changed).
//   A static screen costs no readbacks and produces no new frames, so the
//   preview/encoder downstream also idles.
// - Latest-frame-wins slot with buffer reuse: frames are not queued. If the
//   consumer has not taken the previous frame, its buffer is overwritten in
//   place (no allocation, no growing latency).
// - Software cursor compositing via XFixes: the cursor image is fetched only
//   when its serial changes and alpha-blended (premultiplied ARGB) into the
//   frame, so the cursor appears even though X never includes it in readbacks.

use super::{CaptureEngine, CaptureFrame, CaptureRect};
use crate::window_filter::WindowIdentifier;
use anyhow::{anyhow, Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::protocol::Event;

const DEFAULT_FPS: u32 = 60;

// ---------------------------------------------------------------------------
// Pure helpers (unit tested)
// ---------------------------------------------------------------------------

/// Intersect `region` with a screen of `screen_w` x `screen_h`.
/// Returns `None` if nothing visible remains.
pub fn clip_to_screen(region: CaptureRect, screen_w: u32, screen_h: u32) -> Option<CaptureRect> {
    let x0 = region.x.max(0);
    let y0 = region.y.max(0);
    let x1 = (region.x as i64 + region.width as i64).min(screen_w as i64);
    let y1 = (region.y as i64 + region.height as i64).min(screen_h as i64);
    if x1 <= x0 as i64 || y1 <= y0 as i64 {
        return None;
    }
    Some(CaptureRect::new(x0, y0, (x1 - x0 as i64) as u32, (y1 - y0 as i64) as u32))
}

/// Do two rectangles overlap?
pub fn rects_intersect(a: CaptureRect, b: CaptureRect) -> bool {
    let ax1 = a.x as i64 + a.width as i64;
    let ay1 = a.y as i64 + a.height as i64;
    let bx1 = b.x as i64 + b.width as i64;
    let by1 = b.y as i64 + b.height as i64;
    (a.x as i64) < bx1 && (b.x as i64) < ax1 && (a.y as i64) < by1 && (b.y as i64) < ay1
}

/// Force the X byte of BGRX pixels to 0xFF so the buffer is valid BGRA.
pub fn force_opaque(bgra: &mut [u8]) {
    for px in bgra.chunks_exact_mut(4) {
        px[3] = 0xFF;
    }
}

/// Cached XFixes cursor image (premultiplied ARGB, one u32 per pixel).
#[derive(Clone, Debug, Default)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub xhot: i32,
    pub yhot: i32,
    pub pixels: Vec<u32>,
}

/// Alpha-blend a premultiplied-ARGB cursor into a BGRA frame.
/// `cx`,`cy` are the cursor's pointer position in frame-local coordinates.
pub fn blend_cursor(frame: &mut [u8], fw: u32, fh: u32, stride: u32, cur: &CursorImage, cx: i32, cy: i32) {
    let ox = cx - cur.xhot;
    let oy = cy - cur.yhot;
    for row in 0..cur.height as i32 {
        let fy = oy + row;
        if fy < 0 || fy >= fh as i32 {
            continue;
        }
        for col in 0..cur.width as i32 {
            let fx = ox + col;
            if fx < 0 || fx >= fw as i32 {
                continue;
            }
            let argb = cur.pixels[(row as u32 * cur.width + col as u32) as usize];
            let a = argb >> 24;
            if a == 0 {
                continue;
            }
            let off = (fy as u32 * stride + fx as u32 * 4) as usize;
            let inv = 255 - a;
            // premultiplied: dst = src + dst * (1 - a)
            let sb = argb & 0xFF;
            let sg = (argb >> 8) & 0xFF;
            let sr = (argb >> 16) & 0xFF;
            let px = &mut frame[off..off + 4];
            px[0] = (sb + (px[0] as u32 * inv + 127) / 255).min(255) as u8;
            px[1] = (sg + (px[1] as u32 * inv + 127) / 255).min(255) as u8;
            px[2] = (sr + (px[2] as u32 * inv + 127) / 255).min(255) as u8;
            px[3] = 0xFF;
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state between engine and capture thread
// ---------------------------------------------------------------------------

struct Shared {
    running: AtomicBool,
    new_frame: AtomicBool,
    show_cursor: AtomicBool,
    fps: AtomicU32,
    region: Mutex<CaptureRect>,
    region_dirty: AtomicBool,
    slot: Mutex<Option<CaptureFrame>>,
}

/// MIT-SHM segment backed by a memfd.
struct ShmSegment {
    seg: shm::Seg,
    ptr: *mut u8,
    size: usize,
}

impl ShmSegment {
    fn new<C: Connection>(conn: &C, size: usize) -> Result<Self> {
        // SAFETY: plain libc calls; every result is checked.
        unsafe {
            let fd = libc::memfd_create(c"rustframe-shm".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return Err(anyhow!("memfd_create failed"));
            }
            if libc::ftruncate(fd, size as libc::off_t) != 0 {
                libc::close(fd);
                return Err(anyhow!("ftruncate failed"));
            }
            let ptr = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            if ptr == libc::MAP_FAILED {
                libc::close(fd);
                return Err(anyhow!("mmap failed"));
            }
            let seg = conn.generate_id()?;
            // x11rb takes ownership of the fd and closes it after sending.
            let owned = <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd);
            let attach = conn.shm_attach_fd(seg, owned, false).map(|c| c.check());
            match attach {
                Ok(Ok(())) => Ok(Self { seg, ptr: ptr as *mut u8, size }),
                _ => {
                    libc::munmap(ptr, size);
                    Err(anyhow!("ShmAttachFd failed"))
                }
            }
        }
    }

    fn slice(&self, len: usize) -> &[u8] {
        // SAFETY: mapping is `size` bytes and alive for &self.
        unsafe { std::slice::from_raw_parts(self.ptr, len.min(self.size)) }
    }

    fn destroy<C: Connection>(self, conn: &C) {
        let _ = conn.shm_detach(self.seg);
        let _ = conn.flush();
        // SAFETY: ptr/size came from mmap.
        unsafe {
            libc::munmap(self.ptr as *mut _, self.size);
        }
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Linux (X11) capture engine
pub struct LinuxCaptureEngine {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    is_active: bool,
    region: Option<CaptureRect>,
}

impl LinuxCaptureEngine {
    pub fn new() -> Result<Self> {
        Ok(Self {
            shared: Arc::new(Shared {
                running: AtomicBool::new(false),
                new_frame: AtomicBool::new(false),
                show_cursor: AtomicBool::new(true),
                fps: AtomicU32::new(DEFAULT_FPS),
                region: Mutex::new(CaptureRect::new(0, 0, 0, 0)),
                region_dirty: AtomicBool::new(false),
                slot: Mutex::new(None),
            }),
            thread: None,
            is_active: false,
            region: None,
        })
    }

    /// Set the maximum capture rate (frames are still only produced on damage).
    pub fn set_target_fps(&self, fps: u32) {
        self.shared.fps.store(fps.clamp(1, 240), Ordering::Relaxed);
    }
}

impl Drop for LinuxCaptureEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

impl CaptureEngine for LinuxCaptureEngine {
    fn start(&mut self, region: CaptureRect, show_cursor: bool, _excluded_windows: Option<Vec<WindowIdentifier>>) -> Result<()> {
        // X11 has no per-window capture exclusion; the destination window must
        // be placed outside the region instead.
        self.stop();
        if std::env::var_os("DISPLAY").is_none() {
            return Err(anyhow!("Linux capture requires an X11 DISPLAY (Wayland/PipeWire not yet supported)"));
        }
        // Connect up-front so start() reports failures synchronously.
        let (conn, screen_num) = x11rb::connect(None).context("Failed to connect to X server")?;

        *self.shared.region.lock().unwrap() = region;
        self.shared.show_cursor.store(show_cursor, Ordering::Relaxed);
        self.shared.region_dirty.store(true, Ordering::Relaxed);
        self.shared.running.store(true, Ordering::SeqCst);

        let shared = self.shared.clone();
        self.thread = Some(
            std::thread::Builder::new()
                .name("rustframe-x11-capture".into())
                .spawn(move || {
                    if let Err(e) = capture_loop(conn, screen_num, &shared) {
                        log::error!("[LINUX_CAPTURE] capture thread stopped: {e:#}");
                    }
                    shared.running.store(false, Ordering::SeqCst);
                })?,
        );
        self.region = Some(region);
        self.is_active = true;
        Ok(())
    }

    fn stop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.is_active = false;
        self.shared.new_frame.store(false, Ordering::SeqCst);
        *self.shared.slot.lock().unwrap() = None;
    }

    fn is_active(&self) -> bool {
        self.is_active && self.shared.running.load(Ordering::SeqCst)
    }

    fn has_new_frame(&self) -> bool {
        self.shared.new_frame.load(Ordering::Acquire)
    }

    fn get_frame(&mut self) -> Option<CaptureFrame> {
        let f = self.shared.slot.lock().unwrap().take();
        self.shared.new_frame.store(false, Ordering::Release);
        f
    }

    fn set_cursor_visible(&mut self, visible: bool) -> Result<()> {
        self.shared.show_cursor.store(visible, Ordering::Relaxed);
        self.shared.region_dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn get_region(&self) -> Option<CaptureRect> {
        self.region
    }

    fn update_region(&mut self, region: CaptureRect) -> Result<()> {
        self.region = Some(region);
        *self.shared.region.lock().unwrap() = region;
        self.shared.region_dirty.store(true, Ordering::Release);
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Capture thread
// ---------------------------------------------------------------------------

fn capture_loop<C: Connection>(conn: C, screen_num: usize, shared: &Shared) -> Result<()> {
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let (screen_w, screen_h) = (screen.width_in_pixels as u32, screen.height_in_pixels as u32);
    if screen.root_depth != 24 && screen.root_depth != 32 {
        return Err(anyhow!("Unsupported root depth {}", screen.root_depth));
    }

    // XDamage: get notified about changed screen areas.
    let damage_id = match conn.damage_query_version(1, 1).ok().and_then(|c| c.reply().ok()) {
        Some(_) => {
            let id = conn.generate_id()?;
            conn.damage_create(id, root, damage::ReportLevel::BOUNDING_BOX)?;
            Some(id)
        }
        None => {
            log::warn!("[LINUX_CAPTURE] XDamage unavailable; capturing every tick");
            None
        }
    };
    let has_xfixes = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
    let has_shm = conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some();
    conn.flush()?;

    let mut shm_seg: Option<ShmSegment> = None;
    let mut shm_failed = !has_shm;
    let mut cursor: Option<(u32, CursorImage)> = None; // (serial, image)
    let mut last_cursor_pos = (i32::MIN, i32::MIN);
    let mut dirty = true;

    while shared.running.load(Ordering::SeqCst) {
        let tick = Instant::now();
        let period = Duration::from_secs_f64(1.0 / shared.fps.load(Ordering::Relaxed).max(1) as f64);

        let requested = *shared.region.lock().unwrap();
        if shared.region_dirty.swap(false, Ordering::AcqRel) {
            dirty = true;
        }
        let Some(rect) = clip_to_screen(requested, screen_w, screen_h) else {
            std::thread::sleep(period);
            continue;
        };

        // Drain damage events; only mark dirty if they touch our region.
        while let Some(ev) = conn.poll_for_event()? {
            if let Event::DamageNotify(d) = ev {
                let a = d.area;
                if rects_intersect(rect, CaptureRect::new(a.x as i32, a.y as i32, a.width as u32, a.height as u32)) {
                    dirty = true;
                }
            }
        }
        if let Some(id) = damage_id {
            conn.damage_subtract(id, x11rb::NONE, x11rb::NONE)?;
        } else {
            dirty = true;
        }

        // Cursor: re-fetch image only when the serial changes.
        let show_cursor = shared.show_cursor.load(Ordering::Relaxed) && has_xfixes;
        let mut cursor_pos = None;
        if show_cursor {
            if let Ok(reply) = conn.xfixes_get_cursor_image()?.reply() {
                let pos = (reply.x as i32, reply.y as i32);
                let in_region = pos.0 >= rect.x - 64 && pos.1 >= rect.y - 64
                    && pos.0 < rect.x + rect.width as i32 + 64 && pos.1 < rect.y + rect.height as i32 + 64;
                let serial_changed = cursor.as_ref().map(|(s, _)| *s) != Some(reply.cursor_serial);
                if pos != last_cursor_pos && (in_region || rects_intersect(rect, CaptureRect::new(last_cursor_pos.0 - 64, last_cursor_pos.1 - 64, 128, 128))) {
                    dirty = true;
                }
                if serial_changed && in_region {
                    dirty = true;
                }
                if serial_changed {
                    cursor = Some((reply.cursor_serial, CursorImage {
                        width: reply.width as u32,
                        height: reply.height as u32,
                        xhot: reply.xhot as i32,
                        yhot: reply.yhot as i32,
                        pixels: reply.cursor_image,
                    }));
                }
                last_cursor_pos = pos;
                cursor_pos = Some(pos);
            }
        }

        if dirty {
            let stride = rect.width * 4;
            let len = (stride * rect.height) as usize;

            // Reuse the buffer of an unconsumed frame (latest-frame-wins).
            let mut buf = shared.slot.lock().unwrap().take().map(|f| f.data).unwrap_or_default();
            buf.resize(len, 0);

            let grabbed = grab(&conn, root, rect, &mut shm_seg, &mut shm_failed, &mut buf)?;
            if grabbed {
                force_opaque(&mut buf);
                if let (Some(pos), Some((_, img))) = (cursor_pos, cursor.as_ref()) {
                    blend_cursor(&mut buf, rect.width, rect.height, stride, img, pos.0 - rect.x, pos.1 - rect.y);
                }
                *shared.slot.lock().unwrap() = Some(CaptureFrame {
                    data: buf,
                    width: rect.width,
                    height: rect.height,
                    stride,
                    offset_x: rect.x,
                    offset_y: rect.y,
                    gpu_texture: None,
                });
                shared.new_frame.store(true, Ordering::Release);
                dirty = false;
            }
        }
        conn.flush()?;

        if let Some(rest) = period.checked_sub(tick.elapsed()) {
            std::thread::sleep(rest);
        }
    }

    if let Some(seg) = shm_seg.take() {
        seg.destroy(&conn);
    }
    if let Some(id) = damage_id {
        let _ = conn.damage_destroy(id);
        let _ = conn.flush();
    }
    Ok(())
}

/// Read `rect` from `root` into `out` (tightly packed BGRX). Returns false if
/// the server refused the request (e.g. transient BadMatch during mode change).
fn grab<C: Connection>(
    conn: &C,
    root: u32,
    rect: CaptureRect,
    shm_seg: &mut Option<ShmSegment>,
    shm_failed: &mut bool,
    out: &mut [u8],
) -> Result<bool> {
    let len = out.len();
    if !*shm_failed {
        if shm_seg.as_ref().map_or(true, |s| s.size < len) {
            if let Some(old) = shm_seg.take() {
                old.destroy(conn);
            }
            // Over-allocate a little to avoid reallocating on small resizes.
            match ShmSegment::new(conn, len + len / 4) {
                Ok(s) => *shm_seg = Some(s),
                Err(e) => {
                    log::warn!("[LINUX_CAPTURE] MIT-SHM unavailable ({e}); using GetImage");
                    *shm_failed = true;
                }
            }
        }
        if let Some(seg) = shm_seg.as_ref() {
            let reply = conn
                .shm_get_image(root, rect.x as i16, rect.y as i16, rect.width as u16, rect.height as u16, !0, ImageFormat::Z_PIXMAP.into(), seg.seg, 0)?
                .reply();
            return Ok(match reply {
                Ok(_) => {
                    out.copy_from_slice(seg.slice(len));
                    true
                }
                Err(_) => false,
            });
        }
    }
    match conn.get_image(ImageFormat::Z_PIXMAP, root, rect.x as i16, rect.y as i16, rect.width as u16, rect.height as u16, !0)?.reply() {
        Ok(r) if r.data.len() >= len => {
            out.copy_from_slice(&r.data[..len]);
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_inside_and_outside() {
        assert_eq!(clip_to_screen(CaptureRect::new(10, 10, 100, 50), 1920, 1080), Some(CaptureRect::new(10, 10, 100, 50)));
        assert_eq!(clip_to_screen(CaptureRect::new(-10, 1060, 100, 50), 1920, 1080), Some(CaptureRect::new(0, 1060, 90, 20)));
        assert_eq!(clip_to_screen(CaptureRect::new(2000, 0, 100, 50), 1920, 1080), None);
    }

    #[test]
    fn intersection() {
        let a = CaptureRect::new(0, 0, 10, 10);
        assert!(rects_intersect(a, CaptureRect::new(5, 5, 10, 10)));
        assert!(!rects_intersect(a, CaptureRect::new(10, 0, 5, 5)));
    }

    #[test]
    fn cursor_blend_premultiplied() {
        let mut frame = vec![100u8; 4 * 4 * 4];
        let cur = CursorImage { width: 1, height: 1, xhot: 0, yhot: 0, pixels: vec![0x80_40_00_00] };
        blend_cursor(&mut frame, 4, 4, 16, &cur, 1, 1);
        let off = 16 + 4;
        // B/G: 0 + 100*127/255 ≈ 50; R: 64 + 50 = 114
        assert_eq!(&frame[off..off + 4], &[50, 50, 114, 255]);
        assert_eq!(&frame[0..4], &[100, 100, 100, 100]);
    }

    #[test]
    fn cursor_clipped_offscreen() {
        let mut frame = vec![0u8; 4 * 2 * 2];
        let cur = CursorImage { width: 2, height: 2, xhot: 0, yhot: 0, pixels: vec![0xFF_FF_FF_FF; 4] };
        blend_cursor(&mut frame, 2, 2, 8, &cur, -1, -1);
        assert_eq!(&frame[0..4], &[255, 255, 255, 255]);
        assert_eq!(&frame[4..8], &[0, 0, 0, 0]);
    }
}
