// capture/macos_sck.rs - macOS ScreenCaptureKit capture backend (macOS 12.3+)
//
// This module implements an async capture stream using ScreenCaptureKit so the
// system compositor can include the real cursor (showsCursor) without any
// manual overlay.
//
// IMPORTANT:
// - All Objective-C / ScreenCaptureKit interactions must happen on the main thread.
// - Keep CoreGraphics (screenshot) capture as a fallback in capture/macos.rs.

#![allow(unexpected_cfgs)]
#![allow(unused_unsafe)]
#![allow(clippy::arc_with_non_send_sync)]

use crate::window_filter::WindowIdentifier;
use anyhow::{anyhow, Result};
use block::ConcreteBlock;
use cocoa::base::{id, nil};
use cocoa::foundation::{NSArray, NSUInteger};
use objc::declare::ClassDecl;
use objc::rc::{autoreleasepool, StrongPtr};
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::c_void;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};

#[link(name = "ScreenCaptureKit", kind = "framework")]
extern "C" {
    static SCStreamFrameInfoStatus: *const c_void;
    static SCStreamFrameInfoDirtyRects: *const c_void;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetImageBuffer(sample_buffer: *const c_void) -> *mut c_void;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sample_buffer: *const c_void,
        create_if_necessary: u8,
    ) -> *const c_void;
}

/// CoreMedia CMTime (passed by value to setMinimumFrameInterval:)
#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const KCMTIME_FLAGS_VALID: u32 = 1;
const KCFNUMBER_SINT64_TYPE: isize = 4;

// SCFrameStatus values
const SC_FRAME_STATUS_IDLE: i64 = 1;
const SC_FRAME_STATUS_BLANK: i64 = 2;
const SC_FRAME_STATUS_SUSPENDED: i64 = 3;
const SC_FRAME_STATUS_STOPPED: i64 = 5;

fn frame_interval(fps: u32) -> CMTime {
    CMTime {
        value: 1,
        timescale: fps.clamp(1, 240) as i32,
        flags: KCMTIME_FLAGS_VALID,
        epoch: 0,
    }
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferLockBaseAddress(pixel_buffer: *mut c_void, lock_flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: *mut c_void, lock_flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: *mut c_void) -> u32;
    fn CVPixelBufferGetIOSurface(pixel_buffer: *mut c_void) -> *mut c_void;
}

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    fn IOSurfaceGetID(iosurface: *mut c_void) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRetain(cf: *mut c_void) -> *mut c_void;
    fn CFRelease(cf: *mut c_void);
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, the_type: isize, value_ptr: *mut c_void) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRectMakeWithDictionaryRepresentation(dict: *const c_void, rect: *mut CGRect) -> bool;
}

// libdispatch
#[cfg(target_os = "macos")]
extern "C" {
    static _dispatch_main_q: c_void;

    fn dispatch_sync_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn pthread_main_np() -> i32;

    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
    fn dispatch_semaphore_create(value: isize) -> *mut c_void;
    fn dispatch_semaphore_wait(sema: *mut c_void, timeout: u64) -> isize;
    fn dispatch_semaphore_signal(sema: *mut c_void) -> isize;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

const DISPATCH_TIME_FOREVER: u64 = u64::MAX;
const DISPATCH_QUEUE_PRIORITY_DEFAULT: isize = 0;

const KCVPIXELFORMATTYPE_32BGRA: u32 = 0x42475241; // 'BGRA'
const KCVPIXELBUFFERLOCK_READONLY: u64 = 1;

// SCStreamOutputTypeScreen is 0 in current SDKs.
const SC_STREAM_OUTPUT_TYPE_SCREEN: NSUInteger = 0;

type IOSurfaceInfo = (u32, u32, i64, i64, i64, i64);

struct OutputState {
    latest: Mutex<Option<(Vec<u8>, u32, u32)>>,
    // (iosurface_id, pixel_format, crop_x_px, crop_y_px, crop_w_px, crop_h_px)
    latest_iosurface: Mutex<Option<IOSurfaceInfo>>,
    // Retained IOSurface pointer (kept alive with CFRetain)
    retained_iosurface: Mutex<Option<*mut std::ffi::c_void>>,
    seq: AtomicU64,
    region_x: AtomicI32,
    region_y: AtomicI32,
    region_w: AtomicU32,
    region_h: AtomicU32,
    scale_milli: AtomicU32,

    // --- performance state ---
    /// Max frames per second requested from the compositor
    fps: AtomicU32,
    /// Skip CPU copies when no dirty rect touches the region
    skip_unchanged: AtomicBool,
    /// Stream is configured with sourceRect = region (GPU-side crop), so
    /// delivered buffers already contain exactly the region
    source_rect_active: AtomicBool,
    /// sourceRect failed once; stay in full-display mode
    source_rect_disabled: AtomicBool,
    /// A stream configuration update is in flight
    config_inflight: AtomicBool,
    /// Region (points) the stream configuration currently targets
    applied_region: Mutex<Option<(i32, i32, u32, u32)>>,
    /// Crop rect (pixels) of the last frame copied out, for dirty-rect skipping
    last_crop: Mutex<Option<(i64, i64, i64, i64)>>,
    show_cursor: AtomicBool,
}

impl OutputState {
    fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            latest_iosurface: Mutex::new(None),
            retained_iosurface: Mutex::new(None),
            seq: AtomicU64::new(0),
            region_x: AtomicI32::new(0),
            region_y: AtomicI32::new(0),
            region_w: AtomicU32::new(0),
            region_h: AtomicU32::new(0),
            scale_milli: AtomicU32::new(1000),
            fps: AtomicU32::new(60),
            skip_unchanged: AtomicBool::new(false),
            source_rect_active: AtomicBool::new(false),
            source_rect_disabled: AtomicBool::new(false),
            config_inflight: AtomicBool::new(false),
            applied_region: Mutex::new(None),
            last_crop: Mutex::new(None),
            show_cursor: AtomicBool::new(true),
        }
    }

    fn region_points(&self) -> (i32, i32, u32, u32) {
        (
            self.region_x.load(Ordering::Relaxed),
            self.region_y.load(Ordering::Relaxed),
            self.region_w.load(Ordering::Relaxed),
            self.region_h.load(Ordering::Relaxed),
        )
    }

    /// Take the spare buffer of an unconsumed frame (latest-frame-wins), so the
    /// callback does not allocate a new Vec for every frame.
    fn take_spare(&self) -> Vec<u8> {
        self.latest
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .map(|(d, _, _)| d)
            .unwrap_or_default()
    }

    /// Move the latest frame out (no copy).
    fn take(&self) -> Option<(Vec<u8>, u32, u32, u64)> {
        let seq = self.seq.load(Ordering::Relaxed);
        self.latest
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .map(|(d, w, h)| (d, w, h, seq))
    }

    fn set_region_points(&self, x: i32, y: i32, w: u32, h: u32) {
        self.region_x.store(x, Ordering::Relaxed);
        self.region_y.store(y, Ordering::Relaxed);
        self.region_w.store(w, Ordering::Relaxed);
        self.region_h.store(h, Ordering::Relaxed);
    }

    fn set_scale_milli(&self, scale_milli: u32) {
        self.scale_milli
            .store(scale_milli.max(1), Ordering::Relaxed);
    }

    fn set(&self, data: Vec<u8>, w: u32, h: u32) {
        if let Ok(mut g) = self.latest.lock() {
            *g = Some((data, w, h));
        }
        self.seq.fetch_add(1, Ordering::Relaxed);
    }

    fn set_iosurface(
        &self,
        iosurface_id: u32,
        pixel_format: u32,
        crop_x: i64,
        crop_y: i64,
        crop_w: i64,
        crop_h: i64,
    ) {
        if let Ok(mut g) = self.latest_iosurface.lock() {
            *g = Some((iosurface_id, pixel_format, crop_x, crop_y, crop_w, crop_h));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_iosurface_retained(
        &self,
        iosurface_ptr: *mut std::ffi::c_void,
        iosurface_id: u32,
        pixel_format: u32,
        crop_x: i64,
        crop_y: i64,
        crop_w: i64,
        crop_h: i64,
    ) {
        unsafe {
            // Release old IOSurface if exists
            if let Ok(mut retained) = self.retained_iosurface.lock() {
                if let Some(old_ptr) = *retained {
                    if !old_ptr.is_null() {
                        CFRelease(old_ptr);
                        log::debug!("Released old IOSurface pointer");
                    }
                }

                // Retain new IOSurface
                if !iosurface_ptr.is_null() {
                    CFRetain(iosurface_ptr);
                    *retained = Some(iosurface_ptr);
                    log::debug!("Retained IOSurface pointer: {:?}", iosurface_ptr);
                } else {
                    *retained = None;
                }
            }
        }

        // Store ID and crop info
        if let Ok(mut g) = self.latest_iosurface.lock() {
            *g = Some((iosurface_id, pixel_format, crop_x, crop_y, crop_w, crop_h));
        }
    }

    fn get_iosurface(&self) -> Option<(*mut std::ffi::c_void, u32, u32, i64, i64, i64, i64)> {
        // Get IOSurface info
        let iosurface_info = self.latest_iosurface.lock().ok().and_then(|g| *g)?;

        // Get retained pointer
        let ptr = self.retained_iosurface.lock().ok().and_then(|g| *g)?;

        // CRITICAL: CFRetain here so caller gets a guaranteed valid reference
        // This prevents race condition where SCK releases pointer before caller can retain it
        unsafe {
            CFRetain(ptr);
        }
        tracing::debug!(
            "get_iosurface: Retained pointer {:?} for render thread",
            ptr
        );

        // Return (pointer, id, format, crop_x, crop_y, crop_w, crop_h)
        // Caller MUST CFRelease when done!
        Some((
            ptr,
            iosurface_info.0,
            iosurface_info.1,
            iosurface_info.2,
            iosurface_info.3,
            iosurface_info.4,
            iosurface_info.5,
        ))
    }

    fn seq_value(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }
}

fn responds(obj: id, selector: Sel) -> bool {
    unsafe {
        let ok: bool = msg_send![obj, respondsToSelector: selector];
        ok
    }
}

fn run_on_main_thread<F: FnOnce()>(f: F) {
    unsafe {
        if pthread_main_np() != 0 {
            f();
            return;
        }

        struct Ctx<F: FnOnce()>(Option<F>);
        extern "C" fn trampoline<F: FnOnce()>(ctx: *mut c_void) {
            let ctx = unsafe { &mut *(ctx as *mut Ctx<F>) };
            if let Some(f) = ctx.0.take() {
                f();
            }
        }

        let mut ctx = Ctx(Some(f));
        dispatch_sync_f(
            &_dispatch_main_q as *const c_void,
            &mut ctx as *mut _ as *mut c_void,
            trampoline::<F>,
        );
    }
}

fn ensure_output_class() -> *const Class {
    static mut CLS: *const Class = std::ptr::null();
    static ONCE: std::sync::Once = std::sync::Once::new();

    ONCE.call_once(|| unsafe {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("RustFrameSCStreamOutput", superclass)
            .expect("Failed to declare RustFrameSCStreamOutput");

        decl.add_ivar::<*mut c_void>("rustState");

        extern "C" fn did_output_sample_buffer(
            this: &Object,
            _cmd: Sel,
            _stream: *mut Object,
            sample_buffer: *mut c_void,
            _type: NSUInteger,
        ) {
            log::trace!("[SCK] did_output_sample_buffer called");
            unsafe {
                let state_ptr: *mut c_void = *this.get_ivar("rustState");
                if state_ptr.is_null() {
                    log::warn!("[SCK] rustState is null in delegate");
                    return;
                }
                let state: &OutputState = &*(state_ptr as *const OutputState);

                let pixel_buffer = CMSampleBufferGetImageBuffer(sample_buffer as *const c_void);
                if pixel_buffer.is_null() {
                    return;
                }

                // ScreenCaptureKit can deliver different pixel formats depending on configuration.
                // We only handle 32BGRA here; anything else is ignored (but must not crash).
                let pixel_format = CVPixelBufferGetPixelFormatType(pixel_buffer);
                if pixel_format != KCVPIXELFORMATTYPE_32BGRA {
                    return;
                }

                // Frame status / dirty rects (ScreenCaptureKit attachments)
                let (status, dirty) = frame_info(sample_buffer as *const c_void);
                if matches!(
                    status,
                    Some(SC_FRAME_STATUS_IDLE)
                        | Some(SC_FRAME_STATUS_BLANK)
                        | Some(SC_FRAME_STATUS_SUSPENDED)
                        | Some(SC_FRAME_STATUS_STOPPED)
                ) {
                    return;
                }

                // Extract IOSurface for GPU acceleration (before locking pixel buffer)
                let iosurface = CVPixelBufferGetIOSurface(pixel_buffer);

                // Get crop region (will be calculated below)
                let iosurface_id = if !iosurface.is_null() {
                    Some(IOSurfaceGetID(iosurface))
                } else {
                    None
                };

                if CVPixelBufferLockBaseAddress(pixel_buffer, KCVPIXELBUFFERLOCK_READONLY) != 0 {
                    return;
                }

                let base = CVPixelBufferGetBaseAddress(pixel_buffer) as *const u8;
                let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel_buffer);
                let full_w = CVPixelBufferGetWidth(pixel_buffer) as u32;
                let full_h = CVPixelBufferGetHeight(pixel_buffer) as u32;

                if base.is_null() || bytes_per_row == 0 || full_w == 0 || full_h == 0 {
                    let _ =
                        CVPixelBufferUnlockBaseAddress(pixel_buffer, KCVPIXELBUFFERLOCK_READONLY);
                    return;
                }

                let min_row_bytes = (full_w as usize).saturating_mul(4);
                if bytes_per_row < min_row_bytes {
                    let _ =
                        CVPixelBufferUnlockBaseAddress(pixel_buffer, KCVPIXELBUFFERLOCK_READONLY);
                    return;
                }

                let scale_m = state.scale_milli.load(Ordering::Relaxed).max(1) as i64;
                let rx_pt = state.region_x.load(Ordering::Relaxed) as i64;
                let ry_pt = state.region_y.load(Ordering::Relaxed) as i64;
                let rw_pt = state.region_w.load(Ordering::Relaxed) as i64;
                let rh_pt = state.region_h.load(Ordering::Relaxed) as i64;

                // Convert region from points (top-left origin) to pixels.
                let mut rx_px = ((rx_pt * scale_m) + 500) / 1000;
                let mut ry_px = ((ry_pt * scale_m) + 500) / 1000;
                let mut rw_px = ((rw_pt * scale_m) + 500) / 1000;
                let mut rh_px = ((rh_pt * scale_m) + 500) / 1000;

                // With sourceRect active the compositor already cropped (and scaled)
                // the buffer to the region, so the whole buffer is the region.
                if state.source_rect_active.load(Ordering::Acquire) {
                    rx_px = 0;
                    ry_px = 0;
                    rw_px = full_w as i64;
                    rh_px = full_h as i64;
                }

                if rw_px <= 0 || rh_px <= 0 {
                    // Fallback: full frame
                    rx_px = 0;
                    ry_px = 0;
                    rw_px = full_w as i64;
                    rh_px = full_h as i64;
                }

                // Clamp to bounds.
                if rx_px < 0 {
                    rw_px += rx_px;
                    rx_px = 0;
                }
                if ry_px < 0 {
                    rh_px += ry_px;
                    ry_px = 0;
                }

                let max_w = ((full_w as i64) - rx_px).max(0);
                let max_h = ((full_h as i64) - ry_px).max(0);
                rw_px = rw_px.clamp(0, max_w);
                rh_px = rh_px.clamp(0, max_h);

                let out_w = rw_px as u32;
                let out_h = rh_px as u32;
                let rx_bytes = (rx_px as usize).saturating_mul(4);
                let out_row_bytes = (out_w as usize).saturating_mul(4);
                if rx_bytes.saturating_add(out_row_bytes) > bytes_per_row {
                    let _ =
                        CVPixelBufferUnlockBaseAddress(pixel_buffer, KCVPIXELBUFFERLOCK_READONLY);
                    return;
                }
                if out_w == 0 || out_h == 0 {
                    let _ =
                        CVPixelBufferUnlockBaseAddress(pixel_buffer, KCVPIXELBUFFERLOCK_READONLY);
                    return;
                }

                // Nothing inside the crop changed since the last copied frame: skip.
                let crop = (rx_px, ry_px, rw_px, rh_px);
                if state.skip_unchanged.load(Ordering::Relaxed) {
                    if let Some(rects) = dirty.as_ref() {
                        let same_crop = state.last_crop.lock().map(|g| *g == Some(crop)).unwrap_or(false);
                        if same_crop && !rects.iter().any(|r| rect_intersects(r, crop)) {
                            let _ = CVPixelBufferUnlockBaseAddress(
                                pixel_buffer,
                                KCVPIXELBUFFERLOCK_READONLY,
                            );
                            return;
                        }
                    }
                }

                let mut rgba = state.take_spare();
                rgba.resize((out_w as usize) * (out_h as usize) * 4, 0);
                let out_stride = (out_w as usize) * 4;

                // OPTIMIZED: Bulk copy rows then swap channels in-place
                // Much faster than pixel-by-pixel copy+swap
                for y in 0..(out_h as usize) {
                    let src_y = (ry_px as usize) + y;
                    let src_row = base.add(src_y * bytes_per_row).add(rx_bytes);
                    let dst_row_start = y * out_stride;

                    // Copy entire row at once (BGRA → buffer)
                    std::ptr::copy_nonoverlapping(
                        src_row,
                        rgba.as_mut_ptr().add(dst_row_start),
                        out_stride,
                    );

                    // Swap B and R channels in-place (BGRA → RGBA)
                    let dst_row = &mut rgba[dst_row_start..dst_row_start + out_stride];
                    for chunk in dst_row.chunks_exact_mut(4) {
                        chunk.swap(0, 2); // Swap B <-> R
                    }
                }

                let _ = CVPixelBufferUnlockBaseAddress(pixel_buffer, KCVPIXELBUFFERLOCK_READONLY);

                // Store IOSurface with crop region for GPU rendering (RETAIN it to keep alive)
                if let Some(id) = iosurface_id {
                    if !iosurface.is_null() {
                        state.set_iosurface_retained(
                            iosurface,
                            id,
                            pixel_format,
                            rx_px,
                            ry_px,
                            rw_px,
                            rh_px,
                        );
                    } else {
                        state.set_iosurface(id, pixel_format, rx_px, ry_px, rw_px, rh_px);
                    }
                }

                // Debug: log first few pixels to verify data
                if rgba.len() >= 12 {
                    log::debug!(
                        "SCK frame: {}x{}, first pixel RGBA: [{}, {}, {}, {}]",
                        out_w,
                        out_h,
                        rgba[0],
                        rgba[1],
                        rgba[2],
                        rgba[3]
                    );
                }

                if let Ok(mut g) = state.last_crop.lock() {
                    *g = Some(crop);
                }
                state.set(rgba, out_w, out_h);
            }
        }

        decl.add_method(
            sel!(stream:didOutputSampleBuffer:ofType:),
            did_output_sample_buffer
                as extern "C" fn(&Object, Sel, *mut Object, *mut c_void, NSUInteger),
        );

        CLS = decl.register();
    });

    unsafe { CLS }
}

/// Does a dirty rect (pixels) intersect crop (x, y, w, h in pixels)?
fn rect_intersects(r: &CGRect, crop: (i64, i64, i64, i64)) -> bool {
    let (cx, cy, cw, ch) = (crop.0 as f64, crop.1 as f64, crop.2 as f64, crop.3 as f64);
    r.origin.x < cx + cw
        && cx < r.origin.x + r.size.width
        && r.origin.y < cy + ch
        && cy < r.origin.y + r.size.height
}

/// Read SCStreamFrameInfo status and dirty rects from a sample buffer.
/// Dirty rects are `None` when not reported (treat as fully dirty).
unsafe fn frame_info(sample_buffer: *const c_void) -> (Option<i64>, Option<Vec<CGRect>>) {
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample_buffer, 0);
    if attachments.is_null() || CFArrayGetCount(attachments) < 1 {
        return (None, None);
    }
    let dict = CFArrayGetValueAtIndex(attachments, 0);
    if dict.is_null() {
        return (None, None);
    }

    let status = {
        let num = CFDictionaryGetValue(dict, SCStreamFrameInfoStatus);
        let mut v: i64 = 0;
        if !num.is_null() && CFNumberGetValue(num, KCFNUMBER_SINT64_TYPE, &mut v as *mut i64 as *mut c_void) {
            Some(v)
        } else {
            None
        }
    };

    let rects_arr = CFDictionaryGetValue(dict, SCStreamFrameInfoDirtyRects);
    let dirty = if rects_arr.is_null() {
        None
    } else {
        let n = CFArrayGetCount(rects_arr);
        let mut out = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let d = CFArrayGetValueAtIndex(rects_arr, i);
            let mut r = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
            if d.is_null() || !CGRectMakeWithDictionaryRepresentation(d, &mut r) {
                return (status, None); // unparseable → treat as fully dirty
            }
            out.push(r);
        }
        Some(out)
    };
    (status, dirty)
}

/// Clamp a region (top-left points) to the display and return the
/// sourceRect (points) plus output size (pixels).
fn source_rect_for(region: (i32, i32, u32, u32), scale: f64) -> Option<(CGRect, usize, usize)> {
    let info = crate::display_info::get();
    let (dw, dh) = (info.width_points, info.height_points);
    let x = (region.0 as f64).clamp(0.0, dw);
    let y = (region.1 as f64).clamp(0.0, dh);
    let w = (region.0 as f64 + region.2 as f64).min(dw) - x;
    let h = (region.1 as f64 + region.3 as f64).min(dh) - y;
    if w < 1.0 || h < 1.0 {
        return None;
    }
    let rect = CGRect::new(&CGPoint::new(x, y), &CGSize::new(w, h));
    Some((rect, (w * scale).round() as usize, (h * scale).round() as usize))
}

/// Build a SCStreamConfiguration for the current state.
/// `region` = Some → GPU-side crop via sourceRect; None → full display.
unsafe fn build_config(state: &OutputState, region: Option<(i32, i32, u32, u32)>) -> id {
    let cfg: id = msg_send![class!(SCStreamConfiguration), new];
    if cfg == nil {
        return nil;
    }
    let scale = state.scale_milli.load(Ordering::Relaxed).max(1) as f64 / 1000.0;
    let info = crate::display_info::get();
    let (w, h) = match region.and_then(|r| source_rect_for(r, scale)) {
        Some((rect, w, h)) if responds(cfg, sel!(setSourceRect:)) => {
            let _: () = msg_send![cfg, setSourceRect: rect];
            (w, h)
        }
        _ => (info.width_pixels as usize, info.height_pixels as usize),
    };
    if responds(cfg, sel!(setWidth:)) {
        let _: () = msg_send![cfg, setWidth: w as NSUInteger];
    }
    if responds(cfg, sel!(setHeight:)) {
        let _: () = msg_send![cfg, setHeight: h as NSUInteger];
    }
    if responds(cfg, sel!(setShowsCursor:)) {
        let show: i8 = if state.show_cursor.load(Ordering::Relaxed) { 1 } else { 0 };
        let _: () = msg_send![cfg, setShowsCursor: show];
    }
    if responds(cfg, sel!(setPixelFormat:)) {
        let _: () = msg_send![cfg, setPixelFormat: KCVPIXELFORMATTYPE_32BGRA];
    }
    if responds(cfg, sel!(setQueueDepth:)) {
        let _: () = msg_send![cfg, setQueueDepth: 3 as NSUInteger];
    }
    if responds(cfg, sel!(setMinimumFrameInterval:)) {
        let _: () = msg_send![cfg, setMinimumFrameInterval: frame_interval(state.fps.load(Ordering::Relaxed))];
    }
    cfg
}

struct ConfigUpdate {
    stream: id, // retained
    state: Arc<OutputState>,
    region: Option<(i32, i32, u32, u32)>,
}

extern "C" fn apply_config_update(ctx: *mut c_void) {
    let ctx = unsafe { Box::from_raw(ctx as *mut ConfigUpdate) };
    unsafe {
        autoreleasepool(|| {
            let cfg = build_config(&ctx.state, ctx.region);
            if cfg == nil || !responds(ctx.stream, sel!(updateConfiguration:completionHandler:)) {
                ctx.state.config_inflight.store(false, Ordering::Release);
                if cfg != nil {
                    let _: () = msg_send![cfg, release];
                }
                return;
            }
            let state = ctx.state.clone();
            let region = ctx.region;
            let block = ConcreteBlock::new(move |error: id| {
                if error == nil {
                    if let Ok(mut g) = state.applied_region.lock() {
                        *g = region;
                    }
                    state.source_rect_active.store(region.is_some(), Ordering::Release);
                } else if region.is_some() {
                    log::warn!("[SCK] sourceRect update failed; falling back to full-display capture");
                    state.source_rect_disabled.store(true, Ordering::Release);
                }
                state.config_inflight.store(false, Ordering::Release);
            })
            .copy();
            let _: () = msg_send![ctx.stream, updateConfiguration: cfg completionHandler: &*block];
            let _: () = msg_send![cfg, release];
        });
        let _: () = msg_send![ctx.stream, release];
    }
}

/// Build an NSArray of SCWindow objects to exclude from capture
/// Matches window identifiers against shareable content windows
fn build_excluding_windows_array(excluded: &Option<Vec<WindowIdentifier>>, shareable: id) -> id {
    unsafe {
        let empty = NSArray::array(nil);

        if let Some(ref windows) = excluded {
            if windows.is_empty() {
                return empty;
            }

            // Log what we're excluding
            log::info!(
                "[SCK] Building excludingWindows array for {} windows",
                windows.len()
            );
            for w in windows {
                log::debug!(
                    "[SCK]   Excluding: app_id={}, window_name={}",
                    w.app_id,
                    w.window_name
                );
            }

            // Get all available windows from shareable content
            let windows_list: id = msg_send![shareable, windows];
            if windows_list == nil {
                log::warn!("[SCK] No windows available from shareable content");
                return empty;
            }

            let count: NSUInteger = msg_send![windows_list, count];
            log::debug!("[SCK] Found {} total windows in shareable content", count);

            let array_cls = class!(NSMutableArray);
            let result_array: id = msg_send![array_cls, arrayWithCapacity: windows.len()];

            // Try to match our exclusion list against available windows
            for i in 0..count {
                let sc_window: id = msg_send![windows_list, objectAtIndex: i];
                if sc_window == nil {
                    continue;
                }

                // Get window ID (CGWindowID) from SCWindow
                let window_id: u32 = msg_send![sc_window, windowID];

                // Get window title
                let title: id = msg_send![sc_window, title];
                let title_str = if title != nil {
                    let cstr: *const std::os::raw::c_char = msg_send![title, UTF8String];
                    if !cstr.is_null() {
                        std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };

                // Get owning application (SCRunningApplication)
                let app: id = msg_send![sc_window, owningApplication];
                let bundle_id_str = if app != nil {
                    let bundle_id: id = msg_send![app, bundleIdentifier];
                    if bundle_id != nil {
                        let cstr: *const std::os::raw::c_char = msg_send![bundle_id, UTF8String];
                        if !cstr.is_null() {
                            std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string()
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };

                // Check if this window should be excluded
                for exclusion in windows {
                    let mut should_exclude = false;

                    // Special case: preview window identifier format is "RustFrame Preview {window_id}"
                    if exclusion.window_name.starts_with("RustFrame Preview") {
                        if let Ok(id_str) = exclusion
                            .window_name
                            .strip_prefix("RustFrame Preview ")
                            .ok_or("")
                        {
                            if let Ok(target_id) = id_str.parse::<u32>() {
                                if window_id == target_id {
                                    should_exclude = true;
                                }
                            }
                        }
                    } else {
                        // Generic app window matching by bundle ID and window name
                        // Match by bundle ID
                        let bundle_matches =
                            if !exclusion.app_id.is_empty() && !bundle_id_str.is_empty() {
                                bundle_id_str.contains(&exclusion.app_id)
                                    || exclusion.app_id.contains(&bundle_id_str)
                            } else {
                                false
                            };

                        // Match by window name (exact or partial)
                        let window_matches =
                            if !exclusion.window_name.is_empty() && !title_str.is_empty() {
                                title_str.contains(&exclusion.window_name)
                                    || exclusion.window_name.contains(&title_str)
                            } else {
                                false
                            };

                        // For meaningful match: either both match, or window name matches with app_id present
                        if window_matches
                            && (bundle_matches || !exclusion.window_name.is_empty())
                        {
                            should_exclude = true;
                        }
                    }

                    if should_exclude {
                        log::info!(
                            "[SCK] Excluding window: ID={}, bundle='{}', title='{}'",
                            window_id,
                            bundle_id_str,
                            title_str
                        );
                        let _: () = msg_send![result_array, addObject: sc_window];
                        break;
                    }
                }
            }

            let excluded_count: NSUInteger = msg_send![result_array, count];
            log::info!(
                "[SCK] Created excludingWindows NSArray with {} windows",
                excluded_count
            );
            result_array
        } else {
            log::debug!("[SCK] No excluded windows specified");
            empty
        }
    }
}

pub struct ScreenCaptureKitCapture {
    state: Arc<OutputState>,
    output_obj: Option<StrongPtr>,
    stream: Option<StrongPtr>,
    show_cursor: bool,
}

unsafe impl Send for ScreenCaptureKitCapture {}

impl Drop for ScreenCaptureKitCapture {
    fn drop(&mut self) {
        self.stop();

        // Release retained IOSurface on drop
        unsafe {
            if let Ok(mut retained) = self.state.retained_iosurface.lock() {
                if let Some(ptr) = *retained {
                    if !ptr.is_null() {
                        CFRelease(ptr);
                        log::debug!("Released retained IOSurface on drop");
                    }
                }
                *retained = None;
            }
        }
    }
}

impl ScreenCaptureKitCapture {
    pub fn is_available() -> bool {
        unsafe { Class::get("SCShareableContent").is_some() && Class::get("SCStream").is_some() }
    }

    pub fn new() -> Self {
        Self {
            state: Arc::new(OutputState::new()),
            output_obj: None,
            stream: None,
            show_cursor: true,
        }
    }

    pub fn update_region_points(&self, x: i32, y: i32, w: u32, h: u32) {
        self.state.set_region_points(x, y, w, h);
        self.sync_stream_config();
    }

    pub fn update_scale(&self, scale: f64) {
        let scale_milli = (scale * 1000.0).round().max(1.0) as u32;
        self.state.set_scale_milli(scale_milli);
    }

    pub fn start(
        &mut self,
        region_x: i32,
        region_y: i32,
        region_w: u32,
        region_h: u32,
        show_cursor: bool,
        excluded_windows: Option<Vec<WindowIdentifier>>,
    ) -> Result<()> {
        log::info!(
            "[SCK] start() called: region=({},{}) size={}x{} cursor={} excluded_windows={:?}",
            region_x,
            region_y,
            region_w,
            region_h,
            show_cursor,
            excluded_windows
        );

        let has_perm = unsafe { CGPreflightScreenCaptureAccess() };
        log::info!("[SCK] Screen Recording permission check: {}", has_perm);
        if !has_perm {
            log::error!("[SCK] No Screen Recording permission - aborting");
            return Err(anyhow!(
				"Screen Recording permission is not granted. Enable it in System Settings > Privacy & Security > Screen Recording, then restart the app."
			));
        }

        self.show_cursor = show_cursor;
        self.state
            .set_region_points(region_x, region_y, region_w, region_h);

        if !Self::is_available() {
            log::error!("[SCK] ScreenCaptureKit classes not available (macOS < 12.3?)");
            return Err(anyhow!("ScreenCaptureKit classes are not available"));
        }

        log::info!("[SCK] ScreenCaptureKit is available, initializing stream...");

        let state_ptr: *const OutputState = Arc::as_ptr(&self.state);
        let show_cursor_local = self.show_cursor;
        let mut created_stream: Option<StrongPtr> = None;
        let mut created_output: Option<StrongPtr> = None;
        let mut start_err: Option<String> = None;

        log::info!("[SCK] Dispatching SCK initialization to main thread...");

        // Prepare excluded windows for the closure
        let excluded_for_closure = excluded_windows.clone();

        let try_source_rect = !self.state.source_rect_disabled.load(Ordering::Acquire);
        for use_source_rect in [try_source_rect, false] {
            start_err = None;
            run_on_main_thread(|| unsafe {
                autoreleasepool(|| {
                    log::info!("[SCK] Inside main thread autorelease pool");
                    // Backing scale (points->pixels) for main screen.
                    let screen: id = msg_send![class!(NSScreen), mainScreen];
                    let scale: f64 = if screen != nil {
                        let s: f64 = msg_send![screen, backingScaleFactor];
                        if s > 0.0 {
                            s
                        } else {
                            1.0
                        }
                    } else {
                        1.0
                    };
                    let scale_milli = (scale * 1000.0).round().max(1.0) as u32;
                    (*(state_ptr)).set_scale_milli(scale_milli);

                    let main_display_id = CGMainDisplayID();

                    log::info!("[SCK] Fetching shareable content...");
                    let sema = dispatch_semaphore_create(0);
                    if sema.is_null() {
                        log::error!("[SCK] Failed to create dispatch semaphore");
                        start_err = Some("Failed to create dispatch semaphore".to_string());
                        return;
                    }
                    log::info!("[SCK] Semaphore created: {:p}", sema);

                    let shareable_cls = class!(SCShareableContent);
                    log::info!(
                        "[SCK] SCShareableContent class obtained: {:p}",
                        shareable_cls
                    );

                    let result: Arc<Mutex<(id, id)>> = Arc::new(Mutex::new((nil, nil)));
                    let result_cb = result.clone();
                    let sema_cb = sema;

                    log::info!("[SCK] Creating completion handler block...");
                    let block = ConcreteBlock::new(move |content: id, error: id| {
                        // CRITICAL: Retain the Objective-C objects before storing them
                        // They need to survive beyond the autorelease pool of the callback
                        let retained_content = if content != nil {
                            let _: id = msg_send![content, retain];
                            content
                        } else {
                            nil
                        };
                        let retained_error = if error != nil {
                            let _: id = msg_send![error, retain];
                            error
                        } else {
                            nil
                        };

                        if let Ok(mut g) = result_cb.lock() {
                            *g = (retained_content, retained_error);
                        }
                        unsafe {
                            dispatch_semaphore_signal(sema_cb);
                        }
                    })
                    .copy();
                    log::info!(
                        "[SCK] Block created, calling getShareableContentWithCompletionHandler..."
                    );

                    let _: () =
                        msg_send![shareable_cls, getShareableContentWithCompletionHandler: &*block];
                    log::info!("[SCK] Waiting for shareable content...");
                    let _ = dispatch_semaphore_wait(sema, DISPATCH_TIME_FOREVER);
                    log::info!("[SCK] Shareable content received");
                    let (shareable, shareable_error) = result
                        .lock()
                        .ok()
                        .map(|guard| (guard.0, guard.1))
                        .unwrap_or((nil, nil));

                    if shareable_error != nil {
                        let desc: id = msg_send![shareable_error, localizedDescription];
                        let cstr: *const std::os::raw::c_char = msg_send![desc, UTF8String];
                        let s = if !cstr.is_null() {
                            std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string()
                        } else {
                            "Unknown ScreenCaptureKit error".to_string()
                        };
                        start_err = Some(format!("ScreenCaptureKit getShareableContent failed: {s}"));
                        return;
                    }
                    if shareable == nil {
                        log::error!("[SCK] Shareable content is nil");
                        start_err =
                            Some("ScreenCaptureKit returned null shareable content".to_string());
                        return;
                    }

                    log::info!("[SCK] Shareable content valid, fetching displays...");
                    let displays: id = msg_send![shareable, displays];
                    if displays == nil {
                        start_err =
                            Some("ScreenCaptureKit shareable content has no displays".to_string());
                        return;
                    }

                    let count: NSUInteger = msg_send![displays, count];
                    log::info!("[SCK] Found {} displays", count);
                    let mut target_display: id = nil;
                    for i in 0..count {
                        let d: id = msg_send![displays, objectAtIndex: i];
                        if d == nil {
                            continue;
                        }
                        let did: u32 = msg_send![d, displayID];
                        if did == main_display_id {
                            log::info!("[SCK] Found main display: {}", did);
                            target_display = d;
                            break;
                        }
                    }
                    if target_display == nil {
                        log::error!(
                            "[SCK] Main display {} not found in shareable displays",
                            main_display_id
                        );
                        start_err = Some("ScreenCaptureKit could not find main display".to_string());
                        return;
                    }

                    log::info!("[SCK] Creating content filter...");
                    // Create filter and config with exclusion list
                    let excluding_windows =
                        build_excluding_windows_array(&excluded_for_closure, shareable);
                    let filter: id = msg_send![class!(SCContentFilter), alloc];
                    let filter: id = msg_send![filter, initWithDisplay: target_display excludingWindows: excluding_windows];
                    if filter == nil {
                        start_err = Some("Failed to create SCContentFilter".to_string());
                        return;
                    }

                    // Ensure display info is initialized (required for correct sizing).
                    if !crate::display_info::is_initialized() {
                        log::warn!("[SCK] DisplayInfo not initialized, initializing on main thread...");
                        if let Err(e) = crate::display_info::initialize() {
                            start_err = Some(format!("Failed to initialize display info: {e}"));
                            return;
                        }
                    }

                    let state_ref = &*(state_ptr);
                    state_ref.show_cursor.store(show_cursor_local, Ordering::Relaxed);

                    // Prefer a GPU-side crop: the compositor delivers only the region
                    // (sourceRect, in display points) instead of the full display.
                    let initial_region = if use_source_rect {
                        Some(state_ref.region_points())
                    } else {
                        None
                    };
                    log::info!(
                        "[SCK] Stream config: sourceRect={:?}, fps={}",
                        initial_region,
                        state_ref.fps.load(Ordering::Relaxed)
                    );
                    let cfg = build_config(state_ref, initial_region);
                    if cfg == nil {
                        start_err = Some("Failed to create SCStreamConfiguration".to_string());
                        return;
                    }

                    log::info!("[SCK] Creating SCStream...");
                    let stream: id = msg_send![class!(SCStream), alloc];
                    let stream: id =
                        msg_send![stream, initWithFilter: filter configuration: cfg delegate: nil];
                    if stream == nil {
                        log::error!("[SCK] Failed to create SCStream");
                        start_err = Some("Failed to create SCStream".to_string());
                        return;
                    }
                    log::info!("[SCK] SCStream created successfully");

                    let out_cls = ensure_output_class();
                    if out_cls.is_null() {
                        start_err = Some("Failed to create SCStreamOutput class".to_string());
                        return;
                    }
                    let out: id = msg_send![out_cls, new];
                    if out == nil {
                        start_err = Some("Failed to create SCStreamOutput instance".to_string());
                        return;
                    }

                    let out_obj: &mut Object = &mut *out;
                    out_obj.set_ivar("rustState", state_ptr as *mut c_void);

                    log::info!("[SCK] Adding stream output...");
                    let queue = dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0);
                    let mut err: id = nil;
                    let ok: bool = msg_send![stream, addStreamOutput: out type: SC_STREAM_OUTPUT_TYPE_SCREEN sampleHandlerQueue: queue error: &mut err];
                    log::info!("[SCK] addStreamOutput result: {}", ok);
                    if !ok {
                        let desc: id = if err != nil {
                            msg_send![err, localizedDescription]
                        } else {
                            nil
                        };
                        let cstr: *const std::os::raw::c_char = if desc != nil {
                            msg_send![desc, UTF8String]
                        } else {
                            std::ptr::null()
                        };
                        let s = if !cstr.is_null() {
                            std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string()
                        } else {
                            "Unknown error".to_string()
                        };
                        start_err = Some(format!("SCStream addStreamOutput failed: {s}"));
                        return;
                    }

                    log::info!("[SCK] Starting capture stream...");
                    // Start capture.
                    let sema2 = dispatch_semaphore_create(0);
                    if sema2.is_null() {
                        start_err = Some("Failed to create semaphore for startCapture".to_string());
                        return;
                    }
                    let start_error: Arc<Mutex<id>> = Arc::new(Mutex::new(nil));
                    let start_error_cb = start_error.clone();
                    let sema2_cb = sema2;
                    let block2 = ConcreteBlock::new(move |error: id| {
                        log::info!(
                            "[SCK] startCapture completion handler called, error={:?}",
                            error
                        );
                        // Retain error if not nil to prevent it from being deallocated
                        let retained_error = if error != nil {
                            let _: id = msg_send![error, retain];
                            error
                        } else {
                            nil
                        };
                        if let Ok(mut g) = start_error_cb.lock() {
                            *g = retained_error;
                        }
                        unsafe {
                            dispatch_semaphore_signal(sema2_cb);
                        }
                    })
                    .copy();
                    let _: () = msg_send![stream, startCaptureWithCompletionHandler: &*block2];
                    log::info!("[SCK] Waiting for startCapture completion...");
                    let _ = dispatch_semaphore_wait(sema2, DISPATCH_TIME_FOREVER);
                    log::info!("[SCK] startCapture completed");
                    let start_error_obj = start_error.lock().ok().map(|guard| *guard).unwrap_or(nil);
                    if start_error_obj != nil {
                        let desc: id = msg_send![start_error_obj, localizedDescription];
                        let cstr: *const std::os::raw::c_char = msg_send![desc, UTF8String];
                        let s = if !cstr.is_null() {
                            std::ffi::CStr::from_ptr(cstr).to_string_lossy().to_string()
                        } else {
                            "Unknown error".to_string()
                        };
                        log::error!("[SCK] ❌ startCapture FAILED: {}", s);

                        // Release the retained error
                        let _: () = msg_send![start_error_obj, release];

                        start_err = Some(format!("SCStream startCapture failed: {s}"));
                        return;
                    }

                    log::info!("[SCK] Stream started successfully!");
                    state_ref.source_rect_active.store(initial_region.is_some(), Ordering::Release);
                    if let Ok(mut g) = state_ref.applied_region.lock() {
                        *g = initial_region;
                    }
                    created_stream = Some(StrongPtr::new(stream));
                    created_output = Some(StrongPtr::new(out));

                    // Release the retained objects from completion handler
                    if shareable != nil {
                        let _: () = msg_send![shareable, release];
                    }
                    if shareable_error != nil {
                        let _: () = msg_send![shareable_error, release];
                    }
                });
            });
            if start_err.is_none() || !use_source_rect {
                break;
            }
            log::warn!(
                "[SCK] Start with sourceRect failed ({:?}); retrying with full-display capture",
                start_err
            );
            self.state.source_rect_disabled.store(true, Ordering::Release);
        }

        if let Some(err) = start_err {
            log::error!("[SCK] start() failed: {}", err);
            return Err(anyhow!(err));
        }
        self.stream = created_stream;
        self.output_obj = created_output;
        log::info!("[SCK] start() completed successfully");
        Ok(())
    }

    pub fn stop(&mut self) {
        let stream = self.stream.take();
        let output_obj = self.output_obj.take();
        if stream.is_none() && output_obj.is_none() {
            return;
        }

        run_on_main_thread(|| unsafe {
            autoreleasepool(|| {
                if let Some(stream) = stream {
                    let stream: id = *stream;
                    let sema = dispatch_semaphore_create(0);
                    if !sema.is_null() {
                        let sema_for_block = sema;
                        let block = ConcreteBlock::new(move |_error: *mut Object| {
                            dispatch_semaphore_signal(sema_for_block);
                        })
                        .copy();
                        let _: () = msg_send![stream, stopCaptureWithCompletionHandler: &*block];
                        let _ = dispatch_semaphore_wait(sema, DISPATCH_TIME_FOREVER);
                    }
                }

                // Ensure ObjC objects are released on the main thread.
                drop(output_obj);
            });
        });
    }

    /// Move the latest frame out without copying it.
    pub fn take_latest_frame_rgba(&self) -> Option<(Vec<u8>, u32, u32, u64)> {
        self.state.take()
    }

    /// Frame delivery options; fps applies to the next start / config update.
    pub fn set_frame_options(&self, fps: u32, skip_unchanged: bool) {
        self.state.fps.store(fps.clamp(1, 240), Ordering::Relaxed);
        self.state.skip_unchanged.store(skip_unchanged, Ordering::Relaxed);
    }

    /// Keep the stream's sourceRect in sync with the region. Non-blocking:
    /// at most one configuration update is in flight; the latest region wins.
    pub fn sync_stream_config(&self) {
        let Some(stream) = self.stream.as_ref() else {
            return;
        };
        let state = &self.state;
        let want = if state.source_rect_disabled.load(Ordering::Acquire) {
            None
        } else {
            Some(state.region_points())
        };
        let applied = state.applied_region.lock().ok().and_then(|g| *g);
        if want == applied {
            return;
        }
        if state
            .config_inflight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let stream: id = **stream;
        let ctx = Box::new(ConfigUpdate {
            stream: unsafe { msg_send![stream, retain] },
            state: state.clone(),
            region: want,
        });
        // Async so neither the render thread nor the caller blocks on main.
        unsafe {
            dispatch_async_f(
                &_dispatch_main_q as *const c_void,
                Box::into_raw(ctx) as *mut c_void,
                apply_config_update,
            );
        }
    }

    pub fn latest_iosurface(
        &self,
    ) -> Option<(*mut std::ffi::c_void, u32, u32, i64, i64, i64, i64)> {
        self.state.get_iosurface()
    }

    pub fn latest_seq(&self) -> u64 {
        self.state.seq_value()
    }
}
