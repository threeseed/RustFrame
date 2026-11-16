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

## Linux (X11)
See [technical/linux-capture.md](technical/linux-capture.md): region-only MIT-SHM readback,
XDamage-gated capture (idle when static), latest-frame slot, cached cursor compositing.

## Further opportunities (Windows / macOS)
These were identified by reviewing the current engines and are not yet applied:

- **macOS – GPU-side crop with `sourceRect`.** `capture/macos_sck.rs` captures the full
  display and crops on the CPU. Setting `SCStreamConfiguration.sourceRect` (in *points*,
  display-local, top-left origin) together with `width/height` = region size in *pixels*
  would let WindowServer do the crop and scale, cutting bandwidth by display/region area.
  The previous "invalid parameter" error is consistent with passing pixel units.
- **macOS – skip idle frames.** Check the `SCStreamFrameInfoStatus` attachment and drop
  frames whose status is not `Complete` (e.g. `Idle`), and use `dirtyRects` to skip
  readback when no dirty rect intersects the region.
- **macOS – `minimumFrameInterval`.** Set it from the FPS setting so the compositor
  doesn't deliver frames faster than they are consumed.
- **Windows – `DirtyRegion` / `MinUpdateInterval`.** On Windows 11 (24H2+) the
  `GraphicsCaptureSession.MinUpdateInterval` caps delivery rate and
  `Direct3D11CaptureFrame.DirtyRegions` allows skipping frames with no change inside
  the region (`DirtyRegionMode::ReportOnly`).
- **Windows – staging texture ring.** `capture/windows.rs` maps the staging texture
  immediately after `CopySubresourceRegion`, stalling the CPU on the GPU copy. A ring of
  2–3 staging textures (map the one copied last frame) removes that stall.
- **All – buffer reuse.** CPU frame buffers are allocated per frame (`vec![0u8; ..]`);
  recycling buffers (as the Linux engine does) avoids allocator churn on large regions.
- **All – zero-copy preview.** When a GPU handle is present, render the preview directly
  from the shared texture/IOSurface and skip the CPU readback entirely.
