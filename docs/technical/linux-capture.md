# Linux Region Capture (X11)

`src/capture/linux.rs` implements `CaptureEngine` for X11 sessions (including
XWayland apps only when running under an X11 session). Native Wayland requires
the xdg-desktop-portal ScreenCast + PipeWire path, which is not implemented yet.

## Pipeline

1. A dedicated thread owns its own X connection.
2. Each tick (capped at the target FPS, default 60) it drains `DamageNotify`
   events and checks whether any damaged box intersects the region.
3. Only if the region is dirty (damage, cursor move/shape change, region or
   cursor-setting change) does it read back pixels with `ShmGetImage` for
   **just the region** into a memfd-backed MIT-SHM segment.
4. The X byte is forced to 0xFF (BGRX → BGRA) and the cursor is composited in
   software from the cached XFixes cursor image (premultiplied ARGB).
5. The frame is published to a single latest-frame slot.

## Performance and compositing techniques

| Technique | Effect |
|---|---|
| Region-only readback | Cost scales with shared area, not monitor size |
| MIT-SHM via memfd | No pixel data over the X socket; falls back to `GetImage` |
| XDamage gating | A static region costs zero readbacks and emits no frames |
| Latest-frame-wins slot | No queueing latency; unconsumed buffer reused (no realloc) |
| Cursor serial cache | Cursor bitmap fetched only when its shape changes |
| SHM over-allocation (+25%) | Small resizes during border dragging don't re-create the segment |

## Limitations

- No per-window exclusion on X11: keep the preview window outside the region.
- Multi-screen (Zaphod) setups capture only the default screen; Xinerama/RandR
  multi-monitor works because it is one root window.
- Linux destination (preview) window is still a stub.

## Testing

```bash
xvfb-run -a -s "-screen 0 800x600x24" cargo test --no-default-features --test linux_capture
```
