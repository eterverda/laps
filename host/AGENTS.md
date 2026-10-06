# Laps Host (Rust)

Desktop application for the Laps timing system. Cross-platform: macOS and Linux.

## Architecture

```
┌───────────────────────────────────────────┐
│  UI Layer (egui)                          │
│  - Immediate mode, own cell grid layout   │
│  - Own zoom (egui auto-zoom disabled)     │
│  - Fira Code Nerd Font (2:1 cell aspect)  │
│  - Screens: live view; playback/review,   │
│    planning, stats — planned              │
└──────────────┬────────────────────────────┘
               │
        ┌──────┴───────┐
        │ egui texture │
        └──────▲───────┘
               │ RGBA frame
   ┌───────────┴────────────┐
   │ driver                 │
   │ - camera               │  FrameSource → Pipeline → FrameSink:
   │   capture.rs (traits)  │  pull, one decode per frame, fan-out
   │   v4l2 backend         │  capture thread: jpeg-passthrough /
   │   Camera (facade+pipe) │  YUYV decode → ColorImage slot
   │ - dvr (own muxers)     │  recorder: writer thread, mjpeg
   │   MKV (EBML+Cues)      │  passthrough / yuyv re-encode → container
   │   MOV/MP4 (ISOBMFF)    │
   └────────────────────────┘
```

## UI

- **egui** + **eframe** 0.36 — immediate mode GUI
- Own grid layout: 160×45 cells of 16×32 pt (cell is twice the screen
  pixel size: text rasterizes at 2× resolution, on-screen look unchanged);
  all widgets snapped to the grid
- Own zooming and theming; egui auto-zoom (`zoom_with_keyboard`) disabled,
  theme pinned to dark, native window decorations untouched
- One camera frame → one texture → N viewfinders via UV viewport cropping
- Widgets (all in `src/gui/`): `view::Stripe`/`StripeX2` — background
  plates (cut-corner polygon, corner-hugging), text drawn by the caller
  in a closure; `view::BigButton` — status buttons, hover covers the
  drawn content; `viewfinder::ViewfinderFrame` — plate + label + name;
  `header::Header` — LAPS + date/time for a full-width 2-row rect,
  owns a private `Clock` (time by a format string; `[weekday format:mn]`
  at the end is our placeholder, replaced with a Russian two-letter
  weekday, lowercase)
  our placeholder (replaced with a Russian two-letter weekday, lowercase)
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

- **Layers** (`docs/capture-backend-plan.md`): `driver/camera` — one
  module per domain: `capture.rs` (traits `Capture`/`CaptureSession`/
  `FrameSink`, frame types `Frame`/`FrameData`/`CaptureFormat`/
  `DeviceInfo`) — FrameSource; `mod.rs` — pipeline + `Camera` facade
  for GUI; `v4l2.rs` — Linux backend (raw ioctls via nix macros, ABI
  structs by hand; pure Rust, no cc). Format enumeration never changes
  device state (ENUM_* only, no S_FMT). ABI pinned by compile-time
  size/offset asserts against linux/videodev2.h. Defenses: first frame
  after STREAMON validated by preamble (drained if a fragment),
  corrupted buffers rejected via `V4L2_BUF_FLAG_ERROR`, driver
  timestamps used when filled (zero → dequeue moment). `nokhwa.rs` —
  macOS/Windows backend (behind the trait). Backends import only
  `capture.rs`. `DvrSink` lives in `dvr/sink.rs` (dependency
  `dvr → camera::capture`).
- Errors: `camera::Error` (`Io` / `Config` / `Other`); `Other`
  carries the backend error unaltered.
  Each session logs capture-side fps and frame intervals
  (`camera frames received`, `camera frame interval`) — UI-side fps
  can mask capture problems, the pair separates them
- Terminology: one frame — `jpeg` (`FrameData::Jpeg { buf, len }`),
  stream format — `MJPEG`, a file of frames — an `mjpeg` stream.
  `len` is the SOI..EOI frame length computed by the source (FFD9
  search); `None` — no EOI (broken frame: to screen, not to file)
- MJPEG cameras: the source computes the frame length (FFD9 search,
  `memchr`), the buffer is not copied — DVR writes the `buf[..len]`
  slice; decoded to RGBA for the screen only (**zune-jpeg**)
- YUYV cameras: decode via **yuv** crate (dev-profile opt-level = 2)
- Per-frame latency hot spots: buffer copy and decode; hot crates
  get `opt-level` bumps in dev profile (`zune-jpeg`/`jpeg-rusturbo`/
  `jpeg-encoder`/`memchr` = 3, `yuv`/`epaint` = 2) instead of optimizing
  our own crate in dev
- Camera lifecycle states (`CaptureState`: Starting/Live/Dead) and
  feed states (`FeedState`: Off/Live/Rec) signaled UI-ward via
  `crossbeam_utils::AtomicCell`

## Video Recording (DVR)

Own module `src/driver/dvr/`, no external muxer libraries, no ffmpeg.

- **Passthrough**: camera already emits JPEG; recording = muxing blobs,
  CPU cost ≈ 0. YUYV: RGBA re-encoded to JPEG in the writer thread; codec
  is a compile-time alternative (`jpeg-rusturbo` by default — SIMD;
  `--no-default-features --features jpeg-encoder` switches; the two
  features are mutually exclusive)
- **Containers** (per-camera setting in setup.yaml, `dvr.container`,
  default `mkv`):
  - `mkv` — Matroska/EBML, real per-frame millisecond timestamps
    (TimecodeScale = 1 ms, no fps input), Cues per 5 s cluster, no size limit
  - `mov`/`mp4` — ISOBMFF; common engine in `dvr/mp4.rs` (MovWriter is a
    thin `Flavor::Mov` wrapper), stss lists all frames as sync, co64
    offsets unconditionally, mdat largesize patched at finalize
  - `VideoWriter` trait in `dvr/mod.rs` (`write_frame` / `sync_data` /
    `finalize(self: Box<Self>)`) — implemented by all three writers;
    `create` stays inherent per writer, selection is a `match` on
    `ContainerConfig` in `Recorder::start`
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

- Own reader (mirrors writers): MKV Cues / MOV-MP4 moov tables
  (stts/stsz/co64) parsed once into memory, `read_at` per frame (no seek),
  **zune-jpeg** decode of a single frame
- Scrubbing = index lookup + one JPEG decode (~5 ms); every MJPEG frame is
  a keyframe, no GOP rewinds. Target: faster than VLC, no pipeline flush

## Configuration

- **serde** + **serde_yaml** pinned `=0.9.34` (dtolnay's final release,
  deprecated = frozen, not broken). Unlike serde_yml/noyalib it writes
  valid plain scalars unquoted (`1920x1080`, `30fps`) — noyalib quotes
  any string starting with a digit. Revisit noyalib only if serde_yaml
  breaks — setup files in YAML
- Default setup embedded from `assets/setup.yaml` and loaded once at app
  startup — no tests or other code may depend on its contents;
  config-loaded types carry the `Config` suffix (`CameraConfig`,
  `PadConfig`, `DvrConfig`, …), except `Setup`. Don't rename the
  backend crates' own types (`nokhwa::Camera*` and friends)
- Camera spec canonical string form: `C7-1 1920x1080 @ 30fps [MJPEG]`
  (`CameraConfig: Display/FromStr`, lazy-regex based)

## CLI

- **clap** derive. Subcommand `list-cameras` prints available cameras;
  `list-instances` — локальный список живых инстансов из реестра (без
  сервера); no args → GUI mode
- **Remote control** (`src/remote/mod.rs`): один бинарь управляет
  запущенным инстансом. Вызов: `laps [+pid] +команда [хвост]` — `+pid`
  (не больше одного) адресует конкретный инстанс; без него команда идёт
  единственному запущенному, а при нескольких — ошибка со списком pid.
  Посмотреть живые инстансы: субкоманда `list-instances` (локально, без
  сервера). Реестр инстансов — per-user каталог (через `dirs`), файл на
  инстанс, живость проверяется коннектом, мёртвые подчищаются.
- **Команды**: слово-голова плюс хвост аргументов; при склейке argv в
  строку квотинг сохраняет группировку (`"a b"`, `\` для `"` и `\`), так
  что аргументы с пробелами доезжают целыми. Валидация аргументов —
  на принимающей стороне: ошибка приходит ответом с ненулевым кодом, а
  не обрывом вызова. Исполняют команды обработчики трейта
  `remote::Handler` (`handle(&[String]) -> Option<Response>`): сначала
  текущий экран (`Live` реализует), отклонённое поднимается, `App` —
  верхний уровень и превращает отклонённое в "unknown command".
  Stateless-команды (сейчас `echo`) отвечает сам серверный поток, не
  трогая GUI. Ответ = текст + код выхода: текст на stdout при коде 0,
  на stderr иначе; exit-код процесса = код ответа. Прецеденты команд:
  `toggle-live` / `toggle-rec` (идут в GUI, дергают те же `Live::toggle_*`,
  что и кнопки). Новая stateful-команда = ветка в `handle` нужного
  экрана, сервер менять не нужно.

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
- Performance target: FullHD 60fps (achieved: 60.0 sustained on a
  thermally-throttled laptop). Benchmark hygiene: this machine
  downclocks hard at ~95°C (Tctl) and with `powersave` governor —
  check `sensors` before trusting decode/encode timings
- Audio: out of scope for now

## Known Optimizations (not done yet)

- **Per-frame buffer reuse (pool)** — capture allocates ~8 MB RGBA
  (`zeroed_vec`) + ~125 KB JPEG copy per frame (~240 MB/s churn per
  camera). Plan: small generic `Pool` (40 lines, no new deps); DVR channel
  carries pooled buffers recycled by writer; pixel pool per camera (2-3
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
