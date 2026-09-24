# Laps Host (Rust)

Desktop application for the Laps timing system. Cross-platform: macOS and Linux.

## Architecture

```
┌──────────────────────────────────────────┐
│  UI Layer (egui + eframe)                │
│  - Immediate mode, own cell grid layout  │
│  - Own zoom (egui auto-zoom disabled)    │
│  - Fira Code Nerd Font (2:1 cell aspect) │
│  - Screens: live view; playback/review,  │
│    planning, stats — planned             │
└──────────────┬───────────────────────────┘
               │
        ┌──────┴───────┐
        │ egui texture │
        └──────▲───────┘
               │ RGBA frame
   ┌───────────┴────────────┐
   │ driver                 │
   │ - webcam (nokhwa)      │  capture thread: MJPEG passthrough /
   │ - dvr (own muxers)     │  YUYV decode → ColorImage slot
   │   MKV (EBML+Cues)      │  recorder: writer thread, MJPEG blobs
   │   MP4/MOV (ISOBMFF)    │  → container, no re-encode
   └────────────────────────┘
```

## UI

- **egui** + **eframe** 0.36 — immediate mode GUI
- Own grid layout: 160×45 cells of 8×16 pt; all widgets snapped to the grid
- Own zooming and theming; egui auto-zoom (`zoom_with_keyboard`) disabled,
  theme pinned to dark, native window decorations untouched
- One camera frame → one texture → N viewfinders via UV viewport cropping
- Assets embedded via `src/assets.rs` (fonts, testcard SVG, default setup.yaml)
- **resvg** + **usvg** + **tiny-skia** — render testcard SVG to texture
  (used while camera is off/connecting/dead)

## Window

- app-id `ru.fpvladder.laps`, title `LAPS`
- Hidden titlebar with window buttons shown, fullsize content view
  (macOS-style chrome on both platforms); draggable top area, dblclick
  maximize are NOT implemented — don't assume
- Fullscreen: F11 toggle (`ViewportCommand::Fullscreen`)
- Quit: Ctrl-Q (Cmd-Q on macOS)
- Multi-window (jumbotron via egui viewports) — planned, not implemented

## Video Capture

- **nokhwa** 0.10 (V4L2 on Linux, AVFoundation on macOS, MSMF on Windows)
- MJPEG cameras: frame trimmed to SOI..EOI (`memchr` FFD9 search), JPEG
  blob goes to DVR as-is; decode to RGBA only for screen (**zune-jpeg**)
- YUYV cameras: decode via **yuv** crate (dev-profile opt-level = 2)
- Per-frame latency hot spots: nokhwa buffer copy and decode; hot crates
  get `opt-level` bumps in dev profile (`zune-jpeg`/`zune-core` = 3,
  `memchr` = 3, `epaint` = 2) instead of optimizing our own crate in dev
- Camera lifecycle states (`CameraState`: Starting/Live/Dead) and
  feed states (`FeedState`: Off/Live/Rec) signaled UI-ward via
  `crossbeam_utils::AtomicCell`

## Video Recording (DVR)

Own module `src/driver/dvr/`, no external muxer libraries, no ffmpeg.

- **Passthrough**: camera already emits JPEG; recording = muxing blobs,
  CPU cost ≈ 0. YUYV: RGBA re-encoded to JPEG (jpeg-encoder) in the
  writer thread.
- **Containers** (per-camera setting in setup.yaml, `dvr.container`,
  default `mkv`):
  - `mkv` — Matroska/EBML, real per-frame millisecond timestamps
    (TimecodeScale = 1 ms, no fps input), Cues per 5 s cluster, no size limit
  - `mp4`/`mov` — ISOBMFF; common engine in `dvr/mp4.rs` (MovWriter is a
    thin `Flavor::Mov` wrapper), stss lists all frames as sync, co64
    offsets unconditionally, mdat largesize patched at finalize
  - `VideoWriter` enum in `dvr/mod.rs` — closed set, new container added
    by hand (exhaustive match, won't compile otherwise)
- One file per recording start; **no segment rotation** (documented gap)
- Writer thread + bounded channel (64): overflow = dropped frame + warn
  (capture outranks recording); periodic sync every 5 s; `Drop` detaches
  the thread intentionally (finalize+sync_all in background, no join —
  don't "fix" without reading the comment in `Recorder::drop`)
- File created in `Recorder::start` on the caller's thread, so disk errors
  reach the UI; unrecoverable write errors flip `RecordState{ok:false}`
  and the UI shows REC error
- Timestamp prefix for file names: `time` crate,
  `format_description!` — components must be bracketed (`[year]`), bare
  names emit literals

## Video Playback (planned)

- Own reader (mirrors writers): MKV Cues / MP4-MOV moov tables
  (stts/stsz/co64) parsed once into memory, `read_at` per frame (no seek),
  **zune-jpeg** decode of a single frame
- Scrubbing = index lookup + one JPEG decode (~5 ms); every MJPEG frame is
  a keyframe, no GOP rewinds. Target: faster than VLC, no pipeline flush

## Configuration

- **serde** + **serde_yml** (serde_yaml is deprecated/archived; serde_yml
  is the maintained fork) — setup files in YAML
- Default setup embedded from `assets/setup.yaml` and loaded once at app
  startup — no tests or other code may depend on its contents;
  config-loaded types carry the `Config` suffix (`CameraConfig`,
  `PadConfig`, `DvrConfig`, …), except `Setup`. Don't rename nokhwa's own
  `Camera`/`CameraFormat`/`CameraInfo`
- Camera spec canonical string form: `C7-1 1920x1080 @ 30fps [MJPEG]`
  (`CameraConfig: Display/FromStr`, lazy-regex based)

## CLI

- **clap** derive. Subcommand `list-cameras` prints available cameras;
  no args → GUI mode

## Concurrency

- **crossbeam-channel** (bounded channels), **crossbeam-utils**
  (`AtomicCell` for UI-facing state)
- `std::sync::Mutex` for frame slots; no async runtime, no parking_lot

## Logging

- **log** + **env_logger**. Verbose egui/wgpu logs are tamed at init
  (filter to warn) — keep it that way

## Time

- **time** crate (with `macros`, `local-offset`, `formatting`) —
  file-name timestamps and sidecar data

## Build

```bash
cd host
cargo run
```

## Application Identity

| Context              | Value               | Notes                                                                                            |
| -------------------- | ------------------- | ------------------------------------------------------------------------------------------------ |
| Bundle ID / App ID   | `ru.fpvladder.laps` | Hierarchical, Java-style. Used in macOS `CFBundleIdentifier`, Flatpak `app-id`, D-Bus name, etc. |
| Binary name          | `laps`              | Short, lowercase, no translation                                                                 |
| Display name / Label | `Laps`              | Proper noun, no translation, used in UI titles, `.desktop` `Name=`, macOS `CFBundleName`         |

## Packaging

- **macOS**: `Laps.app` bundle (`CFBundleIdentifier = ru.fpvladder.laps`, `CFBundleName = Laps`, `Info.plist`, icon, embedded binary)
- **Linux**: standalone binary + `.desktop` entry (`Name=Laps`, `Exec=laps`, `Icon=laps`); optional Flatpak (`app-id = ru.fpvladder.laps`)

## Notes

- No external/system dependencies — everything via Cargo; ffmpeg is NOT
  required (containers are our own code)
- Performance target: FullHD 60fps. GPU path not excluded by library choices
- Audio: out of scope for now

## Known Optimizations (not done yet)

- **Per-frame buffer reuse (pool)** — capture allocates ~8 MB RGBA
  (`zeroed_vec`) + ~125 KB JPEG copy per frame (~240 MB/s churn per
  camera). Plan: small generic `Pool` (40 lines, no new deps); DVR channel
  carries pooled buffers recycled by writer; pixel pool per webcam (2-3
  buffers), `decode_frame` takes `&mut [u8]`, UI returns buffer to pool
  after `texture.set`; drop `zeroed_vec` (decode_into overwrites fully).
  Expected: -1..3 ms CPU per frame (memset + mmap/page-fault churn), no
  latency change. Do this when scaling to 2+ cameras; measure with a
  counting global allocator before/after
- **Hardware JPEG on macOS** — if software JPEG encode from YUYV/RGB ever
  becomes a bottleneck for recording on macOS: VideoToolbox
  (`VTCompressionSession`, `kCMVideoCodecType_JPEG`) has a hardware path;
  frames are independent, so plain thread-parallel software encode is the
  first thing to try (4 threads → ~3-4 ms/frame effective)
