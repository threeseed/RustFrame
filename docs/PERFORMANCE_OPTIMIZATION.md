# Performance Notes

This document summarizes current performance-related behavior without hard numbers.

## Key Factors
- Target FPS directly impacts CPU/GPU usage.
- Capture region size impacts memory bandwidth.
- Click highlights add extra rendering work.

## Current Pipeline
- Capture engines provide CPU frames and, when available, GPU texture handles.
- The preview window uses GPU rendering when gpu_acceleration is enabled and a GPU handle is present.
- If GPU rendering is disabled or unavailable, the preview window falls back to CPU rendering.

## Practical Tips
- Lower FPS if CPU usage is high.
- Reduce capture region size if performance is poor.
- Disable click highlights if you do not need them.

## Capture optimisations

Engines receive `FrameOptions { target_fps, skip_unchanged }` before `start`.
`skip_unchanged` is enabled unless click highlights are on (their dissolve animation
needs a steady frame stream). When it is enabled, `get_frame` returns `None` for frames
whose capture region did not change, so the preview is not re-rendered needlessly.

### macOS (ScreenCaptureKit)
- **GPU-side crop.** The stream is configured with `sourceRect` (display points,
  top-left origin) and an output size equal to the region in pixels, so WindowServer
  delivers only the region. Region moves/resizes are applied with
  `updateConfiguration:` asynchronously on the main queue (one update in flight, latest
  region wins). If `sourceRect` is rejected at start or on update, the engine falls back
  to full-display capture with CPU crop for the rest of the session.
- **Frame interval.** `minimumFrameInterval` follows the FPS setting (important on
  120 Hz ProMotion displays).
- **Idle and unchanged frames.** Frames with status Idle/Blank/Suspended/Stopped are
  ignored; frames whose `dirtyRects` don't touch the crop are not copied.
- **No duplicate copies.** The latest frame is moved out of the capture slot instead of
  cloned on every poll, and the slot's unconsumed buffer is reused by the next frame.

### Windows (Windows.Graphics.Capture)
- **Staging texture reuse.** The CPU readback texture is created once per region size
  instead of every frame; rows are copied with one `memcpy` when there is no padding.
- **Newest frame only.** The frame pool is drained each poll and only the newest frame is
  read back, removing queued-frame latency.
- **Dirty regions.** `DirtyRegionMode::ReportOnly` (Windows 11 24H2+); frames whose dirty
  rects miss the region are dropped without readback.
- **`MinUpdateInterval`** follows the FPS setting (Windows 11 24H2+). Both are no-ops on
  older Windows.

### Windows (GDI copy)
- The memory DC and DIB section are cached and recreated only on size change.

### Not done
- **Staging texture ring (Windows).** Mapping the previous frame's copy avoids the
  `Map` stall but adds one frame of latency. With the staging texture reused, the copy
  is a small region and the stall is short, so latency was preferred.
- **Zero-copy preview (Windows).** The GPU path stays disabled: the capture and preview
  use different D3D11 devices, which crashes (see `gpu_acceleration` in
  `capture/windows.rs`). Sharing one device is needed first.
