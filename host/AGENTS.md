# Laps Host (Rust)

Desktop application for the Laps timing system. Cross-platform: macOS and Linux.

## Architecture

```
┌─────────────────────────────────────────┐
│  UI Layer (egui + eframe)               │
│  - Immediate mode, grid-based layout    │
│  - Multiple windows via viewports       │
│  - Monospace fonts, custom styling      │
│  - Screens: live view, playback/review, │
│    flight planning, statistics, etc.    │
└─────────────────────────────────────────┘
       │              │              │
       ▼              ▼              ▼
  ┌─────────┐   ┌──────────┐   ┌──────────┐
  │ Capture │   │ Recorder │   │ Player   │
  │ nokhwa  │   │ video-rs │   │ video-rs │
  └────┬────┘   └──────────┘   └────┬─────┘
       │                            │
       └────────────┬───────────────┘
                    │
               RGBA frames
                    │
             ┌──────┴───────┐
             │ egui texture │
             └──────────────┘
```

## Libraries by Concern

### UI
- **egui** + **eframe** — immediate mode GUI, cross-platform
- **epaint** — 2D rendering primitives (comes with egui)
- Custom scalable grid layout (monospace cell-based, like older Gio version)
- Virtual resolution (e.g. 2560×1440) scaled to physical display, sharp on HiDPI
- Multi-window: main window + jumbotron (and potentially more) via egui viewports
- Native fullscreen: macOS (Spaces), Linux (WM fullscreen) — `ViewportBuilder::with_fullscreen()` + `ViewportCommand::Fullscreen` for toggle
- Window chrome:
  - macOS: native title bar with traffic light buttons
  - Linux: CSD (client-side decorations) styled like Adwaita buttons
- Menu bar: macOS only (native NSMenu via `muda`, polled via `try_recv()` in update loop); Linux uses in-app UI
- Styling: fixed built-in theme (dark), not user-editable. Color palette defined in code

### Video Capture
- **nokhwa** — cross-platform webcam capture (AVFoundation/V4L2/MSMF)
- Capture → RGBA frame → egui texture

### Video Recording
- **video-rs** — encode raw frames from camera and write to container file (Encoder API available since 0.11)
- Recording pipeline:
  1. Capture → encode H.264 → write MPEG-TS segments (segmentation manual if needed)
  2. Concatenate segments: `ffmpeg -i "concat:001.ts|002.ts" -c copy out.mp4`
  3. Remux to MP4/MKV for fast seeking (no re-encoding)
- Texture sharing: one `TextureHandle` uploaded once, displayed in N viewfinders via UV cropping (negligible per-viewfinder cost)

### Video Playback
- **video-rs** — decode from file, seek, scrub
- Frame-accurate seeking for review workflow

## Screens / States

Not all screens use camera or playback:

- **Live** — real-time camera view, recording controls
- **Playback/Review** — recorded video with timeline, scrubbing, mark in/out
- **Flight Planning** — pre-race setup, no video
- **Statistics** — post-race data, no video
- **Jumbotron** — can show live feed or statistics
- **Settings** — configuration

### Keyboard Input
- **global-hotkey** — global shortcuts (Cmd+J, etc.), polled via `try_recv()` in `App::update()`
- egui native — local shortcuts within window
- Text input: forms with text fields, buttons, dynamic validation (egui `TextEdit`, `Button` widgets)
- State management: non-trivial screen states within a window (live, playback, planning, stats, settings, etc.), including nested substates and modal overlays — fully app-managed, egui is rendering-only

### USB/HID Devices
- **hidapi** — HID-class USB devices (works through kernel driver, simple read/write API)
- Only if custom non-HID protocol: **nusb** (pure Rust, but requires kernel driver detachment and manual protocol implementation)

### Concurrency
- **crossbeam** — channels and sync primitives
- **parking_lot** — faster mutexes
- Async (tokio) only if needed for specific I/O, not for the whole application

### Scripting
- **mlua** — Lua 5.1-5.4 / LuaJIT embeddable scripting
- Competition formats (Swiss, qualifiers, double elimination, etc.) described in Lua scripts
- No in-app editor; scripts loaded from files or embedded as defaults

### Logging
- **tklog** — singleton logger, levels, console + file output, optional rotation
- Set up once at startup, use macros everywhere

### Internationalization
- **fluent-zero** — zero-allocation `&'static str` for static text, compile-time PHF cache
- `t!()` macro, `set_lang()` for runtime locale switch
- Translation files in `locales/` (Fluent .ftl format)
- Dynamic text (with variables) returns `Cow<'static, str>` — allocates only on interpolation

### Assets
- **resvg** + **usvg** + **tiny-skia** — render SVG to bitmap for embedded icons/graphics
- Fonts: embedded Fira Code Nerd Font (2:1 aspect ratio, excellent for grid)

### Time
- **chrono** — date/time handling
- For race timing: millisecond accuracy is critical; may need PTP, hardware timestamping, or dedicated timing source beyond wall clock

### CLI
- **clap** — argument parsing via derive macro. Standard Rust ecosystem choice, auto-generates `--help`, completions, subcommands. Example pattern: `#[derive(Parser)] struct Cli { ... }` then `Cli::parse()`
- If no CLI args: launch GUI mode. If args present: execute CLI command and exit

### Configuration
- **serde** + **serde_yaml** — settings storage in YAML files

## Build

```bash
cd host
cargo run
```

## Application Identity

| Context | Value | Notes |
|---------|-------|-------|
| Bundle ID / App ID | `ru.fpvladder.laps` | Hierarchical, Java-style. Used in macOS `CFBundleIdentifier`, Flatpak `app-id`, D-Bus name, etc. |
| Binary name | `laps` | Short, lowercase, no translation |
| Display name / Label | `Laps` | Proper noun, no translation, used in UI titles, `.desktop` `Name=`, macOS `CFBundleName` |

## Packaging

- **macOS**: `Laps.app` bundle (`CFBundleIdentifier = ru.fpvladder.laps`, `CFBundleName = Laps`, `Info.plist`, icon, embedded binary)
- **Linux**: standalone binary + `.desktop` entry (`Name=Laps`, `Exec=laps`, `Icon=laps`); optional Flatpak (`app-id = ru.fpvladder.laps`)

## Dependencies

- **FFmpeg**: system package manager (`brew`, `apt`, etc.) — one external dependency acceptable, everything else via Cargo

## Notes

- `fluent-zero` is young (v0.1.4). If it becomes unmaintained, fallback is `fluent` (Mozilla, mature but allocates) or `rust-i18n` (compile-time, more popular). For personal use the risk is acceptable
- Performance target: FullHD 60fps. GPU path not excluded by library choices
- Audio: out of scope for now
