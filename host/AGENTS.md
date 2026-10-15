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

- **egui** + **eframe** 0.37 — immediate mode GUI
- Own grid layout: 160×45 cells of 16×32 pt (cell is twice the screen
  pixel size: text rasterizes at 2× resolution, on-screen look unchanged);
  all widgets snapped to the grid
- Own zooming and theming; egui auto-zoom (`zoom_with_keyboard`) disabled,
  theme pinned to dark, native window decorations untouched
- One camera frame → one texture → N viewfinders via UV viewport cropping
- Widgets (all in `src/gui/`): `view::Stripe`/`StripeX2` — background
  plates (cut-corner polygon, corner-hugging), text drawn by the caller
  in a closure; `view::NavButton` — status-bar chain button (live +
  scrub EXIT): arrow plate + top text (words, 1-cell gaps, first word
  is an icon, `GLYPH_FLIP` supported) + bottom sub-text (доптаймер,
  aligned to the text after the icon, +3 cols, plate color), 3 states
  (`NavState`: Idle/Active/Disabled — Disabled is outline + text in the
  idle color, `show` returns None), widget 3 cells tall (plate covers
  the top 2), click area = whole widget without the chevron tips;
  generic over `AsRef<str>` text/sub; `view::TransportButton` — scrub
  transport (REW/PLAY/FF/RPT): filled rect, icon centered in the top
  2 cells, sub centered in the 3rd with 1-cell side insets, 2 states
  (`TransportState`: Idle/Active), colors passed in, `interact`/`paint`
  split (seek logic between them); `view::arrow_plate` — the plate
  geometry shared by NavButton (per-edge `PlateEdge` + `PlatePaint`,
  outline drawn StrokeKind::Inside); `viewfinder::ViewfinderFrame` —
  plate + label + name; `header::Header` — LAPS + date/time for a
  full-width 2-row rect, owns a private `Clock` (time by a format
  string; `[weekday format:mn]` at the end is our placeholder, replaced
  with a Russian two-letter weekday, lowercase)
  our placeholder (replaced with a Russian two-letter weekday, lowercase)
- Glyph mirroring: a word prefixed with the vendor marker
  `view::GLYPH_FLIP` (PUA U+E6C0, no glyph in the font) draws its first
  glyph mirrored horizontally — Nerd Fonts has no mirrored icon pairs
  (exit-to-app only points right), so the flip is done at home: the
  glyph quad is re-emitted from the font atlas (`TextureId::Managed(0)`)
  with u mirrored. The marker is stripped before any layout/measuring
  and never reaches epaint
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
- Camera lifecycle states (`CaptureState`: Starting/Live/Dead) signaled
  UI-ward via `crossbeam_utils::AtomicCell`. Кнопки LIVE/REC/RACE в
  live-экране живут в общей машине состояний `Session`
  (`gui/live.rs`): Idle → LiveBefore → RecordHead → RaceCountdown →
  Race → RecordTail → LiveAfter → Done; до старта гонки отжатия шагают
  назад, после — только вперёд, Done терминальное. Доступность кнопок
  по фазам — в юнит-тесте `session_flags_and_availability`

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
  - `mov`/`mp4` — ISOBMFF; one `IsobmffWriter` in `dvr/mp4.rs` for both
    (`create_mov`/`create_mp4` differ in ftyp brands only), stss lists all
    frames as sync, co64 offsets unconditionally, mdat largesize patched at
    finalize
  - `VideoWriter` trait in `dvr/mod.rs` (`write_frame` / `sync_data` /
    `finalize(self: Box<Self>)`) — implemented by both writers; `create`
    stays inherent per writer, selection is a `match` on
    `ContainerConfig` in `Recorder::start` (mkv → MkvWriter, mov/mp4 →
    IsobmffWriter::create_mov/create_mp4)
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

## Video Playback

- **Scrub mode** (`laps scrub`): playback of the latest per-camera recording
  from `captures/`; same header/viewfinder layout as live. Bottom row:
  EXIT plate (icon-only, passive), timeline with hold-to-scrub (hold ≠
  pause) between EXIT and the clock, big clock at the same x as live's
  RACE counter, transport (REW/PLAY/FF/RPT) right of the clock. Screen
  layouts are per-screen private (copy-paste); only view-level geometry
  (`plate_rect`, grid cells) is shared
- Own readers (mirror writers): **MKV** (Cues / cluster scan, `mkv.rs`) and
  **MOV/MP4** (ISOBMFF: stts/stsz/stco|co64 parsed once into memory,
  `mp4.rs` `IsobmffReader` — one reader for both). Readers cover
  our own files only: single 'jpeg' video track, 1 sample per chunk,
  version-0 boxes; foreign layout is rejected with an error, not guessed
- `read_frame_into` per frame (no streaming pipeline), **zune-jpeg** decode
  of a single frame; scrubbing = index lookup + one JPEG decode (~5 ms);
  every MJPEG frame is a keyframe, no GOP rewinds
- Player (`driver/player/`): facade mirrors `Camera` — io lives in a
  worker thread (free function, like `capture_loop`), the facade holds
  texture, frame slot, state atomics and a command channel. Opening is
  async: `open()` returns immediately, index + first frame are built in
  the thread (`PlayerState::Opening/Ready/Failed`); commands sent before
  readiness wait in the channel queue. Container timestamps are the
  clock, no resampling; frame-accurate `seek`; playback-tested at
  30 fps FullHD. The player is a dumb frame supplier: which frame to
  show and when is decided by the playhead, the time model lives
  entirely in `driver/player/playhead.rs`
- Playhead (`Playhead`): the single time-travel model — position =
  base + ∫speed, a pure struct owned by the player's worker thread
  (never the UI thread: UI frame quantization must not affect
  playback), covered by unit tests. UI sends intents (`Play`/`Pause`/
  `Seek`/`SetSpeed`, wheel x0.25–x2 — instant, cancel any gesture;
  `Shuttle`/`ShuttleRelease` — the only source of speed ramps,
  ease-out, landing commits seamlessly by re-anchoring at the landing
  position; shuttle is the jog/shuttle term: скоростная прокрутка с
  разгоном, не путать с дискретным seek);
  the worker maps position → frame index via its pts map, paces
  decodes by frame deadlines (`MIN_PRESENT_SPAN` gate at high speeds;
  backpressure: slot unclaimed by UI — no decode at all, no wasted
  work for occluded windows; under/over-decode counters logged per
  session). Release during play ramps back to the play speed; release
  from pause is an instant stop on the current frame (no drift —
  precise scrubbing). Timeline drag = discrete `Seek` per frame,
  sticks to the cursor, no ramps. Two API layers: real-time wrappers
  (`play()`, `seek()`…) for the worker and an `_at(now)` core —
  private, tests drive it with a fake clock (instant, ms-exact).
  Circle (`Circle { center, radius }`, the LOOP/RPT button): looping
  in VIDEO time (radius ±1.5 s covers 3 s of content at any play
  speed); wrap is pure position arithmetic in the playhead, active
  only in clean play (shuttle/drag run free, center follows any
  rewind); on disable time continues from the cursor position. The
  worker detects wraps via position jumps (sticky catch-up) and draws
  the knob as a stadium spanning the circle
- Remote: `rec`, `race`, `play`, `ff`, `rew` (button presses, same
  code as the clicks), `seek <ms>` (absolute seek of all players),
  `exit` (to menu) — see `Flight::handle_remote`

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
  `list-instances` — local list of live instances from the registry
  (no server involved); no args → GUI mode
- **Remote control** (`src/remote/mod.rs`): the same binary controls a
  running instance. Invocation: `laps [+pid] +command [tail]` — `+pid`
  (at most one) addresses a specific instance; without it the command
  goes to the single running instance, and with several — an error
  listing the pids. List live instances: the `list-instances`
  subcommand (local, no server). Instance registry — per-user dir
  (via `dirs`), one file per instance, liveness by connect, dead ones
  cleaned up.
- **Commands**: a head word plus a tail of args; joining argv into a
  string re-quotes so grouping survives (`"a b"`, `\` for `"` and `\`),
  args with spaces arrive whole. Argument validation — on the receiving
  side: errors come back as a reply with a non-zero code, not as a
  broken call. The server executes commands via a callback
  `Arc<dyn Fn(Vec<String>) -> Response + Send + Sync>` (blocks up to
  `RESPONSE_TIMEOUT`): the closure in `App::new` enqueues the command
  for the GUI, wakes egui via `ctx.request_repaint()` (otherwise the
  answer would wait for the next frame) and waits on a oneshot channel.
  The accept thread serves clients one at a time. Commands are handled
  by inherent `handle_remote` methods: the current screen gets the
  command first (`Flight`), declined commands bubble up, `App` is the
  top level (echo is answered there, everything else becomes "unknown
  command"). Response = text + exit code: text to stdout on code 0, to
  stderr otherwise; the process exit code = the response code.
  Precedent commands: `rec` / `race` (go to the GUI, call the same
  `Flight::toggle_*` as the buttons). A new stateful command =
  a branch in `handle_remote` of the right screen; the server needs no
  changes.

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

- Docs and comments: Russian and English are both fine, but a single
  document/comment (and docs of one structure or kind) must stick to
  one language — no mixing. AGENTS.md is English.
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
