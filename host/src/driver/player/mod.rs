//! Проигрывание своих записей (MJPEG в MKV). Фасад зеркалит `Camera`:
//! `open(path, ctx)` — RAII-контрол, `update_frame() -> bool` — работа
//! на проход, `texture() -> TextureId` — стабильный id. Тайминги между
//! кадрами святы: темп задают timestamp'ы контейнера, быстрее своих
//! таймингов не играем, отставание — честная ресинхронизация.
//!
//! Память постоянная: индекс при open даёт максимальный размер JPEG —
//! один буфер на все кадры; декод идёт in-place в `Arc<ColorImage>`
//! (один буфер на всю жизнь), в egui-дельту уходит клон Arc — копий
//! пикселей на кадр нет вовсе.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::driver::camera::decode::decode_pixels_mjpeg_into;
use crate::driver::camera::fps::FrameStats;
use crate::driver::dvr::{open_reader, VideoReader};

/// Отставание от графика больше этого — не догоняем серией кадров,
/// а перепривязываем часы (резко, честно).
const RESYNC_AFTER: Duration = Duration::from_millis(66);

pub struct Player {
    reader: Box<dyn VideoReader>,
    /// pts всех кадров — копия индекса для борьбы с borrow'ами.
    pts: Vec<u64>,
    ctx: egui::Context,
    texture: egui::TextureHandle,
    dims: crate::driver::dvr::Resolution,
    /// Буфер под JPEG кадр, размер — максимальный в файле.
    jpeg: Vec<u8>,
    /// Пиксельный буфер кадра; дельте egui отдаётся Arc-клон.
    image: Arc<egui::ColorImage>,
    /// Следующий к показу / сейчас на экране (индексы в pts).
    cursor: usize,
    shown_idx: usize,
    playing: bool,
    /// (момент старта, pts на экране на тот момент) — часы воспроизведения.
    anchor: Option<(Instant, u64)>,
    /// Показано кадров за сеанс воспроизведения (лог на паузе).
    presented: u32,
    presented_t0: Instant,
    /// Длительности декода кадров.
    decode: FrameStats,
    /// Длительности чтения кадра (seek + read в JPEG-буфер).
    read: FrameStats,
    /// Сколько раз часы перепривязывались по отставанию.
    resyncs: u32,
}

impl Player {
    /// Открыть файл: индекс, буферы, первый кадр на экране.
    pub fn open(path: &Path, ctx: egui::Context) -> io::Result<Self> {
        let reader = open_reader(path)?;
        let dims = reader.resolution();
        let pts: Vec<u64> = (0..reader.frame_count()).map(|i| reader.timestamp(i)).collect();
        let jpeg = vec![0u8; reader.max_frame_len()];
        let image = Arc::new(egui::ColorImage::new(
            [dims.width as usize, dims.height as usize],
            vec![egui::Color32::TRANSPARENT; (dims.width * dims.height) as usize],
        ));
        // 1×1-пустышка, как у Camera: id валиден с рождения, размер
        // возьмёт первый кадр.
        let texture = ctx.load_texture(
            "player",
            egui::ColorImage::new([1, 1], vec![egui::Color32::TRANSPARENT]),
            egui::TextureOptions::NEAREST,
        );
        let mut me = Self {
            reader,
            pts,
            ctx,
            texture,
            dims,
            jpeg,
            image,
            cursor: 0,
            shown_idx: 0,
            playing: false,
            anchor: None,
            presented: 0,
            presented_t0: Instant::now(),
            decode: FrameStats::default(),
            read: FrameStats::default(),
            resyncs: 0,
        };
        me.read_and_present(0)?;
        log::info!(
            "player: {} frames, {} ms span, max frame {} bytes",
            me.pts.len(),
            me.pts.last().copied().unwrap_or(0),
            me.reader.max_frame_len()
        );
        Ok(me)
    }

    pub fn play(&mut self) {
        if !self.playing && self.cursor < self.pts.len() {
            self.playing = true;
            self.presented = 0;
            self.presented_t0 = Instant::now();
            self.anchor = Some((Instant::now(), self.pts[self.shown_idx]));
            self.ctx.request_repaint();
        }
    }

    pub fn pause(&mut self) {
        if self.playing {
            let elapsed = self.presented_t0.elapsed();
            log::info!(
                "player: {} frames in {:?} -> {:.1} fps",
                self.presented,
                elapsed,
                self.presented as f32 / elapsed.as_secs_f32().max(0.001)
            );
            if let Some(s) = self.decode.snapshot() {
                log::info!("player: jpeg→rgba decode duration: {s}");
            }
            if let Some(s) = self.read.snapshot() {
                log::info!("player: frame read duration: {s}");
            }
            if self.resyncs > 0 {
                log::info!("player: {} clock resyncs", self.resyncs);
            }
        }
        self.playing = false;
        self.anchor = None;
    }

    pub fn toggle(&mut self) {
        if self.playing {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Позиция в файле: pts кадра на экране, мс от начала.
    pub fn position_ms(&self) -> u64 {
        self.pts[self.shown_idx]
    }

    /// Текстура последнего кадра. Id стабилен с open().
    pub fn texture(&self) -> egui::TextureId {
        self.texture.id()
    }

    /// Один проход: показывает не больше одного кадра — следующий, если
    /// его дедлайн наступил. true — на экране новый кадр (fps-замер).
    pub fn update_frame(&mut self) -> io::Result<bool> {
        let Some((wall, base_pts)) = self.anchor else {
            return Ok(false);
        };
        if self.cursor >= self.pts.len() {
            return Ok(false);
        }
        let now_pts = base_pts + wall.elapsed().as_millis() as u64;
        let next_pts = self.pts[self.cursor];
        if next_pts > now_pts {
            // Рано: пробуждение ровно на дедлайн.
            self.ctx
                .request_repaint_after(Duration::from_millis(next_pts - now_pts));
            return Ok(false);
        }
        if Duration::from_millis(now_pts.saturating_sub(self.pts[self.shown_idx])) > RESYNC_AFTER {
            // Отставание: пропускаем устаревшее без показа, часы заново.
            while self.cursor + 1 < self.pts.len()
                && self.pts[self.cursor + 1].saturating_add(33) <= now_pts
            {
                self.cursor += 1;
            }
            self.anchor = Some((Instant::now(), self.pts[self.shown_idx]));
            self.resyncs += 1;
        }

        let i = self.cursor;
        self.read_and_present(i)?;
        self.shown_idx = i;
        self.cursor = i + 1;
        self.presented += 1;
        if self.cursor >= self.pts.len() {
            self.pause(); // конец файла — стоп-кадр
        } else {
            let wait = self.pts[self.cursor].saturating_sub(now_pts);
            self.ctx.request_repaint_after(Duration::from_millis(wait));
        }
        Ok(true)
    }

    /// Кадр i: read в постоянный JPEG-буфер, декод in-place в image.
    fn read_and_present(&mut self, i: usize) -> io::Result<()> {
        let t0 = Instant::now();
        let (_, len) = self.reader.read_frame_into(i, &mut self.jpeg)?;
        self.read.on_process(t0.elapsed());
        let data = std::mem::take(&mut self.jpeg);
        self.present(&data[..len]);
        self.jpeg = data;
        Ok(())
    }

    /// Декод в Arc-буфер in-place; дельте — клон Arc (копии пикселей
    /// нет). Дельта ещё жива (референс сверх ожиданий) — свежий буфер,
    /// это редкий fallback, не горячий путь.
    fn present(&mut self, data: &[u8]) {
        let (w, h) = (self.dims.width, self.dims.height);
        let t0 = Instant::now();
        let decoded = match Arc::get_mut(&mut self.image) {
            Some(image) => decode_pixels_mjpeg_into(data, w, h, &mut image.pixels).is_ok(),
            None => false,
        };
        self.decode.on_process(t0.elapsed());
        if !decoded {
            let mut image = egui::ColorImage::new(
                [w as usize, h as usize],
                vec![egui::Color32::TRANSPARENT; (w * h) as usize],
            );
            if decode_pixels_mjpeg_into(data, w, h, &mut image.pixels).is_ok() {
                self.image = Arc::new(image);
            } else {
                log::warn!("player: frame decode failed");
                return;
            }
        }
        self.texture
            .set(self.image.clone(), egui::TextureOptions::NEAREST);
    }
}
