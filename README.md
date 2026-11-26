# cnc-ddraw — Rust / wgpu experiment

This branch builds a 32-bit Windows `ddraw.dll` from Rust, plus a 64-bit
`ddraw-host.exe` presentation process. A DirectDraw 1–7 client calls
`DirectDrawCreate`, `DirectDrawCreateEx`, or COM `CoCreateInstance`, binds its
HWND with `SetCooperativeLevel`, sets a logical display mode, and creates
primary or offscreen surfaces.

The 32-bit DLL owns all DirectDraw semantics and CPU surface storage. On Wine
it can hand complete primary-surface frames to the 64-bit host through a
shared-memory ring; the host presents them with wgpu DX12. On macOS Wine this
is the **D3DMetal** path: the host is a 64-bit process, so CrossOver can load
the 64-bit D3DMetal `d3d12.dll`/`dxgi.dll`, which a 32-bit process cannot use.

The legacy in-process wgpu renderer is kept as a fallback for native Windows
and for `presenter=inproc`.

## Architecture

```
32-bit game (gamemd.exe)
  ddraw.dll
    DirectDraw 1-7, CPU surfaces, GDI, clipper, palette
    HostWindow (borderless fullscreen / windowed)
    FrameSink  ── shared memory + named events ──┐
    (no GPU device in the 32-bit process)       │
                                                 ▼
64-bit ddraw-host.exe
    wgpu 27 / DX12
    D3DMetal (CrossOver) or native Windows D3D12
    click-through overlay window
    raw pixels + palette/gamma LUT -> WGSL -> swapchain
```

The protocol lives in `protocol/src/lib.rs`. Its header is fixed-width and
pointer-free, so the layout is identical on i686 and x86_64. Three rotating
frame slots prevent the host from observing a torn frame; the palette/gamma
LUT is sent only when its generation changes.

## Build

Install Rust and the Visual Studio C++ x86/x64 tools with the Windows SDK. From
the **x86 Native Tools Command Prompt**, run:

```bat
rustup target add i686-pc-windows-msvc x86_64-pc-windows-msvc
cargo build --locked --release -p cnc-ddraw --target i686-pc-windows-msvc
cargo build --locked --release -p ddraw-host --target x86_64-pc-windows-msvc
```

Outputs:

```
target\i686-pc-windows-msvc\release\ddraw.dll
target\x86_64-pc-windows-msvc\release\ddraw-host.exe
```

Place **both** files beside a 32-bit DirectDraw executable. The host must be
x86_64; 32-bit D3DMetal does not exist.

CI (`.github/workflows/build.yml`) builds both targets on **Ubuntu** with the
**LLVM-MinGW UCRT** toolchain and the `i686-pc-windows-gnullvm` /
`x86_64-pc-windows-gnullvm` Rust targets. No Windows runner is required for the
build. The gnullvm build produces `ddraw.dll` and `ddraw-host.exe`; it does not
produce a PDB.

To cross-compile locally on macOS or Linux, put the LLVM-MinGW UCRT toolchain's
`bin` directory on PATH and run:

```sh
rustup target add i686-pc-windows-gnullvm x86_64-pc-windows-gnullvm
cargo build --locked --release -p cnc-ddraw --target i686-pc-windows-gnullvm
cargo build --locked --release -p ddraw-host --target x86_64-pc-windows-gnullvm
```

To load the local DLL in Wine, use the native DLL override
`WINEDLLOVERRIDES=ddraw=n`. Set `presenter=host` or leave the default
`presenter=auto` (host is selected on Wine when `ddraw-host.exe` is present).

## macOS / CrossOver + D3DMetal

1. Build both artifacts above and place `ddraw.dll` and `ddraw-host.exe` in the
   game directory.
2. Enable **D3DMetal** for the bottle (CrossOver's Advanced/Graphics toggle, or
   `WINED3DMETAL=1` in `cxbottle.conf`).
3. Set `presenter=host` and `renderer=dx12` in `ddraw.ini`.
4. Keep `MTL_HUD_ENABLED=0` unless you are debugging the Metal layer.

The 32-bit game stays on Wine's `win32on64` path; only `ddraw-host.exe` uses
the 64-bit D3DMetal path. If D3DMetal cannot initialise, the host logs the
failure to `ddraw-host.log` and exits; the DLL reports the failed attachment in
`cnc-ddraw.log`.

## Configuration (`ddraw.ini`)

Place an optional `ddraw.ini` next to `ddraw.dll` (or the game executable)
using standard `[ddraw]` keys (with optional `[<exe_name>]` per-game overrides):

```ini
[ddraw]
windowed=false           ; start in windowed mode instead of borderless fullscreen
border=true              ; show window titlebar/frame in windowed mode
resizable=true           ; allow dragging window edges to resize
width=0                  ; windowed client width (0 = follow game logical resolution)
height=0                 ; windowed client height (0 = follow game logical resolution)
posX=-32000              ; windowed X position (-32000 = center on monitor)
posY=-32000              ; windowed Y position (-32000 = center on monitor)
maintas=true             ; preserve aspect ratio (false = stretch to fill window)
vsync=false              ; true = Fifo vsync, false = low-latency Mailbox/Immediate
maxfps=0                 ; 0 = coalesced ~250 Hz cap, >0 = explicit FPS cap
renderer=auto            ; auto, dx12, or vulkan (host backend; WGPU_BACKEND wins)
presenter=auto           ; auto, host, or inproc (auto = host on Wine if available)
keytogglefullscreen=0x0D ; Alt+key toggle between borderless fullscreen and windowed (0 = off)
singlecpu=true           ; pin game-directory threads to CPU0 to prevent multi-core engine freezes
noactivateapp=false      ; shield focus deactivation messages (auto-enabled for RA2/TS presets)
log=false                ; write diagnostic events to cnc-ddraw.log (or set CNC_DDRAW_LOG=path)
```

`presenter=host` never creates a wgpu device in the 32-bit process. With
`presenter=auto`, the host is used only when `ddraw-host.exe` exists and the
process is running under Wine; native Windows keeps the in-process renderer.
`renderer=vulkan` asks the host for wgpu's Vulkan backend (MoltenVK under Wine);
`renderer=dx12` is the D3DMetal path.

Press **Alt+Enter** at any time to toggle between borderless fullscreen and
windowed mode.

## Implementation

- `src/draw/mod.rs` implements `IDirectDraw`, `IDirectDraw2`, `IDirectDraw4`, and
  `IDirectDraw7`, along with `DirectDrawCreate`, `DirectDrawCreateEx`,
  `DirectDrawCreateClipper`, device/mode/surface enumeration, vblank/scanline
  timing, and coalesced frame flushing.
- `src/remote.rs` owns the 32-bit side of the presentation bridge: named
  shared memory, named events, `ddraw-host.exe` launch/monitoring, frame copies,
  LUT updates, and `presented` pacing.
- `src/window/` encapsulates all Win32 windowing concerns: borderless
  fullscreen / windowed lifecycle, Alt+Enter, input mapping, PE32 IAT hooks,
  WndProc subclassing, and `WH_GETMESSAGE`.
- `src/surface.rs` implements `IDirectDrawSurface`, `IDirectDrawSurface2`,
  `IDirectDrawSurface3`, `IDirectDrawSurface4`, `IDirectDrawSurface7`, and
  `IDirectDrawGammaControl`. It owns CPU-backed pixels for primary, offscreen,
  multi-backbuffer flip chains, and overlay surfaces, supporting rectangular
  locks, color/depth fills, ROP fills, same- and cross-format blits,
  stretching, mirroring, source/destination color keys, clipper clipping,
  `BltBatch`, private data, uniqueness/priority/LOD state, gamma ramps, and GDI
  drawing through a DIB.
- `src/clipper.rs`, `src/palette.rs`, and `src/exports.rs` implement the
  DirectDraw clipper, 256-entry palette, export ordinals, thread lock, helper
  exports, and COM class factories.
- `src/renderer.rs` is the legacy 32-bit wgpu presenter. `presenter=host` does
  not instantiate it.
- `host/src/presenter.rs` is the 64-bit wgpu DX12 presenter. The 32-bit side
  converts the raw primary surface (indexed/RGB555/RGB565/XRGB) to tightly
  packed RGBA8 using the palette LUT.

  The **default** present path is a CPU blit: the host nearest-neighbour
  upscales that RGBA8 frame to the letterboxed viewport, writes it into a
  `Bgra8Unorm` texture and `copy_texture_to_texture`s it into the swapchain
  image. It uses no render pipeline, shader, bind group or sampler, because on
  wgpu 27 / DX12 / D3DMetal a draw through a pipeline that declares a bind
  group produces no pixels at all - even when the fragment shader ignores the
  bound resources (`CNC_DDRAW_HOST_CONST_FS` returns a constant green and still
  renders nothing), while the same fullscreen triangle drawn with an empty
  pipeline layout works.

  `CNC_DDRAW_HOST_GPU=1` selects the textured render-pipeline path instead:
  `host/src/present.wgsl` samples an `Rgba8Unorm` frame with `textureLoad` from
  `@builtin(position)`, with texture size and viewport origin in a small uniform
  buffer, so it never calls `textureDimensions()`. There is deliberately no
  sampler binding: naga's HLSL backend turns a sampler into a sampler-heap
  resource array that D3DMetal's FXC rejects with `E5017`.

Host-only presentation diagnostics (set in the environment of
`ddraw-host.exe`, all off by default; the drawing ones need
`CNC_DDRAW_HOST_GPU=1`):

| Variable | Effect |
| --- | --- |
| `CNC_DDRAW_HOST_GPU=1` | Use the textured render-pipeline path instead of the CPU blit |
| `CNC_DDRAW_HOST_CLEAR=red\|green\|blue\|white\|magenta\|0xRRGGBB` | Clear the swapchain and skip drawing |
| `CNC_DDRAW_HOST_SOLID_DRAW=1` | GPU path: draw a fullscreen red triangle with no bindings |
| `CNC_DDRAW_HOST_SOLID=1` | Upload an all-red RGBA frame instead of the game frame |
| `CNC_DDRAW_HOST_CONST_FS=1` | GPU path: frame pipeline, but a constant green fragment shader |
| `CNC_DDRAW_HOST_CONST_NOBIND=1` | GPU path: same vertex shader and constant green fragment, but an **empty** pipeline layout and no bind group |
| `CNC_DDRAW_HOST_POSITION_UV=1` | GPU path: sample from the interpolated vertex UV |
| `CNC_DDRAW_HOST_FULL_VIEWPORT=1` | Stretch the frame over the whole target |

On the first GPU-path present the host also copies the first texture row back
and logs `gpu row0: ...`, which distinguishes "upload never reached the GPU"
from "draw/sample path broken".
- `host/src/overlay.rs` owns the 64-bit click-through overlay window. It is
  `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT`, answers
  `WM_NCHITTEST` with `HTTRANSPARENT`, and is hidden when the game is
  minimised, not foreground, or showing a dialog.
- `protocol/src/lib.rs` is the shared frame-ring header. It uses only 32-bit
  fields and atomics so i686 and x86_64 agree on layout.

The implemented DirectDraw COM objects are single-threaded. Release the display
and its surfaces, or detach the HWND with `SetCooperativeLevel(NULL,
DDSCL_NORMAL)`, before destroying the window. Direct3D 3D game rendering
remains outside this experiment's scope.

## Verify

The SDK smoke test is a separate C executable. It loads the DLL next to the
executable by path, resolves exports by name and ordinal, and calls the
interfaces using the SDK's x86 COM declarations.

```bat
cargo test --locked -p cnc-ddraw
cl /nologo tests\smoke.c user32.lib gdi32.lib /Fe:target\i686-pc-windows-msvc\release\smoke.exe
target\i686-pc-windows-msvc\release\smoke.exe
target\i686-pc-windows-msvc\release\smoke.exe --present
```

With `presenter=auto` and no `ddraw-host.exe` beside the test executable, the
smoke test exercises the legacy in-process renderer, as before. To test the
host path explicitly, copy `ddraw-host.exe` next to the test executable and set
`presenter=host`; the host writes `ddraw-host.log`.

The default run checks 8/16/32-bit surfaces, odd-width pitches, palettes, GDI
writes, overlapping blits, busy surfaces, flipping, DD2/4/7 and Surface2/3/4/7
interfaces, clippers, gamma control, multi-backbuffer chains, Blt FX/ROP/
batching, overlays, vblank/scanline queries, export ordinals, thread lock, and
COM class factories.

## Known limitations

- `presenter=host` is a separate-process presenter, not a cross-process
  DirectDraw implementation: `Lock()` pointers, blits and clippers stay in the
  32-bit process.
- The 64-bit host currently presents only the primary surface. DirectDraw
  overlay surfaces used by some video players (including Bink in some builds)
  must be composited by the 32-bit side before submitting.
- D3DMetal is supplied by CrossOver/GPTK and is not redistributable with this
  project. `ddraw-host.exe` must run in an environment where D3DMetal is
  enabled.
- The host uses a click-through overlay window instead of a cross-process
  swapchain on the game HWND. This keeps D3DMetal inside one process, but the
  overlay must track the game window's position, visibility, focus and dialogs.
- If the host exits, the 32-bit side logs the failure and retries on the next
  present tick; on native Windows it can still fall back to the in-process
  renderer.
- The default host present path is a CPU nearest-neighbour upscale followed by
  `copy_texture_to_texture`, because on wgpu 27 / DX12 / D3DMetal any draw
  through a pipeline that declares a bind group produces no pixels (see the
  presenter notes above). This costs roughly one 1440x1080 RGBA rescale per
  *new* game frame on the CPU, and only when the frame or the window size
  changes. `CNC_DDRAW_HOST_GPU=1` still selects the render-pipeline path for
  environments where descriptor bindings work.
- Frame output is nearest-neighbour scaled; there is no bilinear filtering in
  the CPU path yet, so non-integer scale factors (e.g. 800x600 to 1440x1080)
  show slightly hard pixel edges.
