//! MP4-муксер (ISOBMFF) для одного MJPEG-видеопотока. Общий движок
//! семейства: MP4/MOV — одни и те же боксы, различаются брендами ftyp
//! (см. `Flavor`); MOV-обёртка — в `mov.rs`. Кадры — self-contained JPEG,
//! все ключевые, stss пишем явно. Реальные миллисекундные таймкоды
//! (timescale 1000): длительность sample k — это rel[k+1]-rel[k], последний
//! повторяет предыдущую дельту, переменный frame rate отражается честно,
//! fps на входе не нужен. Индекс для seek'а — co64+stsz+stts в moov
//! (moov после mdat, ридер читает индекс за один проход). Файл живой до
//! finalize() (патч largesize mdat), живучесть к крашу — через sync_data().

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::VideoWriter;

/// Таймлайн в миллисекундах: дельты таймкодов пишем без округления.
const TIMESCALE: u32 = 1000;

const BRAND_ISOM: [u8; 4] = *b"isom";
const BRAND_ISO2: [u8; 4] = *b"iso2";
const BRAND_MP41: [u8; 4] = *b"mp41";
const BRAND_QT: [u8; 4] = *b"qt  ";

/// Вариант контейнера семейства ISOBMFF: отличаются только брендами ftyp.
#[derive(Clone, Copy)]
pub(crate) enum Flavor {
    Mov,
    Mp4,
}

impl Flavor {
    fn major_brand(self) -> [u8; 4] {
        match self {
            Flavor::Mov => BRAND_QT,
            Flavor::Mp4 => BRAND_ISOM,
        }
    }

    fn compat_brands(self) -> &'static [[u8; 4]] {
        match self {
            Flavor::Mov => &[BRAND_QT],
            // major brand дублируется в compat — так принято у MP4.
            Flavor::Mp4 => &[BRAND_ISOM, BRAND_ISO2, BRAND_MP41],
        }
    }
}

// --- Бинарные примитивы ------------------------------------------------------

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

/// Бокс: 32-битный size + тип + payload. Боксов больше 4 ГиБ у нас нет:
/// mdat пишется largesize, moov — килобайты.
fn push_box(out: &mut Vec<u8>, typ: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&be32(payload.len() as u32 + 8));
    out.extend_from_slice(typ);
    out.extend_from_slice(payload);
}

/// Новый payload FullBox с заданными version/flags.
fn fullbox(version: u8, flags: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(version);
    p.extend_from_slice(&flags.to_be_bytes()[1..]);
    p
}

/// Единичная матрица трансформации трека (16.16 / 2.30 fixed point).
const MATRIX: [u32; 9] = [
    0x0001_0000,
    0,
    0, //
    0,
    0x0001_0000,
    0, //
    0,
    0,
    0x4000_0000,
];

/// 'und' — undefined language (ISO 639-2/T packed).
const LANGUAGE_UND: u16 = 0x55C4;

/// Общий движок ISOBMFF. Публичные обёртки (Mp4Writer/MovWriter) держат его
/// и транслируют вызовы; здесь — вся логика.
pub(crate) struct IsobmffCore {
    writer: BufWriter<File>,
    /// Позиция 8-байтного largesize mdat (патч в finalize).
    mdat_size_pos: u64,
    mdat_data_start: u64,
    width: u32,
    height: u32,
    /// Абсолютный таймкод первого кадра: якорь относительных таймкодов.
    first_ts: Option<u64>,
    /// Относительные таймкоды кадров, мс (ровно по одному на кадр).
    /// Дельты для stts выводятся сдвигом: длительность sample k —
    /// это rel[k+1]-rel[k].
    rel_ts: Vec<u64>,
    /// Размеры кадров и их абсолютные оффсеты (таблицы stsz/co64).
    sizes: Vec<u32>,
    offsets: Vec<u64>,
}

impl IsobmffCore {
    pub(crate) fn create(path: &Path, width: u32, height: u32, flavor: Flavor) -> io::Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut writer = BufWriter::new(file);

        // ftyp.
        let brands = flavor.compat_brands();
        let payload_len = 8 + 4 * brands.len();
        writer.write_all(&be32(payload_len as u32 + 8))?;
        writer.write_all(b"ftyp")?;
        writer.write_all(&flavor.major_brand())?;
        writer.write_all(&be32(0x200))?; // minor version
        for brand in brands {
            writer.write_all(brand)?;
        }

        // mdat: size=1 → 64-битный largesize, реальный размер — патч
        // в finalize (стандартный приём, как у ffmpeg без faststart).
        writer.write_all(&be32(1))?;
        writer.write_all(b"mdat")?;
        let mdat_size_pos = writer.stream_position()?;
        writer.write_all(&0u64.to_be_bytes())?;
        let mdat_data_start = writer.stream_position()?;

        Ok(Self {
            writer,
            mdat_size_pos,
            mdat_data_start,
            width,
            height,
            first_ts: None,
            rel_ts: Vec::new(),
            sizes: Vec::new(),
            offsets: Vec::new(),
        })
    }

    /// Пишет кадр в mdat; ts_ms — момент кадра, мс с Unix-эпохи
    /// (внутри — относительно первого кадра).
    pub(crate) fn write_frame(&mut self, data: &[u8], ts_ms: u64) -> io::Result<()> {
        let first = *self.first_ts.get_or_insert(ts_ms);
        let rel = ts_ms.saturating_sub(first);
        let offset = self.writer.stream_position()?;
        self.writer.write_all(data)?;
        self.rel_ts.push(rel);
        self.sizes.push(data.len() as u32);
        self.offsets.push(offset);
        Ok(())
    }

    /// Периодический flush + sync для живучести к крашу.
    pub(crate) fn sync_data(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    /// Патчит largesize mdat, пишет moov (весь индекс — в памяти), flush +
    /// sync_all.
    pub(crate) fn finalize(mut self) -> io::Result<()> {
        self.writer.flush()?;
        let file = self.writer.get_ref();
        let file_size = file.metadata()?.len();
        // largesize mdat — размер всего бокса: 16-байтный заголовок + payload.
        let mdat_start = self.mdat_data_start - 16;
        file.write_all_at(&(file_size - mdat_start).to_be_bytes(), self.mdat_size_pos)?;

        let moov = self.build_moov();
        self.writer.write_all(&moov)?;
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }

    /// Длительность таймлайна: до последнего кадра + его дельта.
    fn total_duration_ms(rel: &[u64]) -> u64 {
        match rel.len() {
            0 | 1 => 0,
            n => {
                let last_delta = rel[n - 1] - rel[n - 2];
                rel[n - 1] + last_delta
            }
        }
    }

    /// stts: длительность sample k = rel[k+1]-rel[k], последний повторяет
    /// предыдущую дельту. Равные соседние дельты схлопываем в (count, delta).
    fn stts_entries(rel: &[u64]) -> Vec<(u32, u32)> {
        let n = rel.len();
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(n);
        for k in 0..n {
            let d = if k + 1 < n {
                rel[k + 1] - rel[k]
            } else if n > 1 {
                rel[n - 1] - rel[n - 2]
            } else {
                0
            };
            let d = u32::try_from(d).unwrap_or(u32::MAX);
            match out.last_mut() {
                Some(e) if e.1 == d => e.0 += 1,
                _ => out.push((1, d)),
            }
        }
        out
    }

    /// Собирает moov: mvhd + trak(tkhd, mdia(mdhd, hdlr, minf(vmhd, dinf,
    /// stbl(stsd, stts, stsc, stsz, co64, stss)))).
    fn build_moov(&self) -> Vec<u8> {
        let n = self.sizes.len();
        let duration = u32::try_from(Self::total_duration_ms(&self.rel_ts)).unwrap_or(u32::MAX);

        let mut moov = Vec::new();

        // mvhd: единый таймлайн файла.
        let mut p = fullbox(0, 0);
        p.extend_from_slice(&be32(0)); // creation time
        p.extend_from_slice(&be32(0)); // modification time
        p.extend_from_slice(&be32(TIMESCALE));
        p.extend_from_slice(&be32(duration));
        p.extend_from_slice(&be32(0x0001_0000)); // rate 1.0
        p.extend_from_slice(&be16(0x0100)); // volume 1.0
        p.extend_from_slice(&be16(0)); // reserved
        p.extend_from_slice(&be32(0)); // reserved
        p.extend_from_slice(&be32(0)); // reserved
        for v in MATRIX {
            p.extend_from_slice(&be32(v));
        }
        for _ in 0..6 {
            p.extend_from_slice(&be32(0)); // pre_defined
        }
        p.extend_from_slice(&be32(2)); // next_track_ID
        push_box(&mut moov, b"mvhd", &p);

        // trak/tkhd: track_ID 1, без аудио (volume 0), размеры в 16.16.
        let mut tkhd = fullbox(0, 0x000007);
        tkhd.extend_from_slice(&be32(0)); // creation time
        tkhd.extend_from_slice(&be32(0)); // modification time
        tkhd.extend_from_slice(&be32(1)); // track_ID
        tkhd.extend_from_slice(&be32(0)); // reserved
        tkhd.extend_from_slice(&be32(duration));
        tkhd.extend_from_slice(&be32(0)); // reserved
        tkhd.extend_from_slice(&be32(0)); // reserved
        tkhd.extend_from_slice(&be16(0)); // layer
        tkhd.extend_from_slice(&be16(0)); // alternate group
        tkhd.extend_from_slice(&be16(0)); // volume (видео)
        tkhd.extend_from_slice(&be16(0)); // reserved
        for v in MATRIX {
            tkhd.extend_from_slice(&be32(v));
        }
        tkhd.extend_from_slice(&be32(self.width << 16));
        tkhd.extend_from_slice(&be32(self.height << 16));
        let mut trak = Vec::new();
        push_box(&mut trak, b"tkhd", &tkhd);

        // mdia/mdhd.
        let mut mdhd = fullbox(0, 0);
        mdhd.extend_from_slice(&be32(0)); // creation time
        mdhd.extend_from_slice(&be32(0)); // modification time
        mdhd.extend_from_slice(&be32(TIMESCALE));
        mdhd.extend_from_slice(&be32(duration));
        mdhd.extend_from_slice(&be16(LANGUAGE_UND));
        mdhd.extend_from_slice(&be16(0)); // pre_defined
        let mut mdia = Vec::new();
        push_box(&mut mdia, b"mdhd", &mdhd);

        // mdia/hdlr: video handler.
        let mut hdlr = fullbox(0, 0);
        hdlr.extend_from_slice(&be32(0)); // pre_defined
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&be32(0)); // reserved
        hdlr.extend_from_slice(&be32(0)); // reserved
        hdlr.extend_from_slice(&be32(0)); // reserved
        hdlr.extend_from_slice(b"VideoHandler\0");
        push_box(&mut mdia, b"hdlr", &hdlr);

        // minf: vmhd + dinf + stbl.
        let mut minf = Vec::new();
        let mut vmhd = fullbox(0, 1); // flags 1 — без альфа-канала
        vmhd.extend_from_slice(&be16(0)); // graphicsmode
        for _ in 0..3 {
            vmhd.extend_from_slice(&be16(0)); // opcolor
        }
        push_box(&mut minf, b"vmhd", &vmhd);

        // dinf/dref: единственный источник — сам файл (url flags=1).
        let mut dref = fullbox(0, 0);
        dref.extend_from_slice(&be32(1)); // entry_count
        push_box(&mut dref, b"url ", &fullbox(0, 1));
        push_box(&mut minf, b"dinf", &dref);

        let mut stbl = Vec::new();

        // stsd: один sample entry 'jpeg' (Photo-JPEG).
        let mut entry = Vec::new();
        entry.extend_from_slice(&[0; 6]); // reserved
        entry.extend_from_slice(&be16(1)); // data_reference_index
        entry.extend_from_slice(&be16(0)); // pre_defined
        entry.extend_from_slice(&be16(0)); // reserved
        for _ in 0..3 {
            entry.extend_from_slice(&be32(0)); // pre_defined
        }
        entry.extend_from_slice(&be16(self.width as u16));
        entry.extend_from_slice(&be16(self.height as u16));
        entry.extend_from_slice(&be32(0x0048_0000)); // horizresolution 72dpi
        entry.extend_from_slice(&be32(0x0048_0000)); // vertresolution
        entry.extend_from_slice(&be32(0)); // reserved
        entry.extend_from_slice(&be16(1)); // frame_count
        entry.extend_from_slice(&[0; 32]); // compressorname
        entry.extend_from_slice(&be16(0x0018)); // depth 24bpp
        entry.extend_from_slice(&be16(0xFFFF)); // pre_defined -1
        let mut jpeg_entry = Vec::new();
        push_box(&mut jpeg_entry, b"jpeg", &entry);
        let mut stsd = fullbox(0, 0);
        stsd.extend_from_slice(&be32(1)); // entry_count
        stsd.extend_from_slice(&jpeg_entry);
        push_box(&mut stbl, b"stsd", &stsd);

        // stts: реальные дельты таймкодов.
        let entries = Self::stts_entries(&self.rel_ts);
        let mut stts = fullbox(0, 0);
        stts.extend_from_slice(&be32(entries.len() as u32));
        for (count, delta) in entries {
            stts.extend_from_slice(&be32(count));
            stts.extend_from_slice(&be32(delta));
        }
        push_box(&mut stbl, b"stts", &stts);

        // stsc: 1 sample на chunk → chunk offset = offset кадра.
        let mut stsc = fullbox(0, 0);
        stsc.extend_from_slice(&be32(1)); // entry_count
        stsc.extend_from_slice(&be32(1)); // first_chunk
        stsc.extend_from_slice(&be32(1)); // samples_per_chunk
        stsc.extend_from_slice(&be32(1)); // sample_description_index
        push_box(&mut stbl, b"stsc", &stsc);

        // stsz: sample_size 0 — размеры из таблицы.
        let mut stsz = fullbox(0, 0);
        stsz.extend_from_slice(&be32(0));
        stsz.extend_from_slice(&be32(n as u32));
        for &s in &self.sizes {
            stsz.extend_from_slice(&be32(s));
        }
        push_box(&mut stbl, b"stsz", &stsz);

        // co64 безусловно (файлы > 4 ГиБ описываются без веток).
        let mut co64 = fullbox(0, 0);
        co64.extend_from_slice(&be32(n as u32));
        for &off in &self.offsets {
            co64.extend_from_slice(&off.to_be_bytes());
        }
        push_box(&mut stbl, b"co64", &co64);

        // stss: все кадры ключевые, пишем явно.
        let mut stss = fullbox(0, 0);
        stss.extend_from_slice(&be32(n as u32));
        for i in 1..=n as u32 {
            stss.extend_from_slice(&be32(i));
        }
        push_box(&mut stbl, b"stss", &stss);

        push_box(&mut minf, b"stbl", &stbl);
        push_box(&mut mdia, b"minf", &minf);
        push_box(&mut trak, b"mdia", &mdia);
        push_box(&mut moov, b"trak", &trak);

        let mut out = Vec::new();
        push_box(&mut out, b"moov", &moov);
        out
    }
}

/// Писатель MP4: ftyp(isom) + mdat + moov.
pub struct Mp4Writer {
    core: IsobmffCore,
}

impl Mp4Writer {
    pub fn create(path: &Path, width: u32, height: u32) -> io::Result<Self> {
        Ok(Self {
            core: IsobmffCore::create(path, width, height, Flavor::Mp4)?,
        })
    }
}

impl VideoWriter for Mp4Writer {
    /// `ts_ms` — момент кадра, мс с Unix-эпохи; внутри файла — относительно
    /// первого кадра.
    fn write_frame(&mut self, data: &[u8], ts_ms: u64) -> io::Result<()> {
        self.core.write_frame(data, ts_ms)
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.core.sync_data()
    }

    fn finalize(self: Box<Self>) -> io::Result<()> {
        self.core.finalize()
    }
}

/// Мини-парсер боксов для тестов семейства (mp4.rs, mov.rs).
#[cfg(test)]
pub(crate) mod boxparse {
    #[derive(Clone)]
    pub(crate) struct Bx {
        pub(crate) typ: [u8; 4],
        pub(crate) start: usize, // начало payload
        pub(crate) len: usize,
    }

    /// Разобрать боксы уровня; largesize (size==1) поддержан — им пишется
    /// mdat.
    pub(crate) fn parse(buf: &[u8], from: usize, to: usize) -> Vec<Bx> {
        let mut out = Vec::new();
        let mut p = from;
        while p < to {
            let size = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap()) as u64;
            let typ: [u8; 4] = buf[p + 4..p + 8].try_into().unwrap();
            let (total, hdr) = if size == 1 {
                (
                    u64::from_be_bytes(buf[p + 8..p + 16].try_into().unwrap()),
                    16usize,
                )
            } else {
                (size, 8)
            };
            out.push(Bx {
                typ,
                start: p + hdr,
                len: total as usize - hdr,
            });
            p += total as usize;
        }
        out
    }

    pub(crate) fn find<'a>(bx: &'a [Bx], typ: &[u8; 4]) -> &'a Bx {
        bx.iter()
            .find(|b| &b.typ == typ)
            .unwrap_or_else(|| panic!("box {:?} not found", String::from_utf8_lossy(typ)))
    }

    /// Первый дочерний бокс заданного типа, по значению — удобно в цепочках
    /// без промежуточных привязок.
    pub(crate) fn child(buf: &[u8], parent: &Bx, typ: &[u8; 4]) -> Bx {
        find(&parse(buf, parent.start, parent.start + parent.len), typ).clone()
    }

    /// u32 из payload бокса по смещению (payload начинается с start).
    pub(crate) fn u32v(buf: &[u8], b: &Bx, at: usize) -> u32 {
        u32::from_be_bytes(buf[b.start + at..b.start + at + 4].try_into().unwrap())
    }

    pub(crate) fn u64v(buf: &[u8], b: &Bx, at: usize) -> u64 {
        u64::from_be_bytes(buf[b.start + at..b.start + at + 8].try_into().unwrap())
    }

    pub(crate) fn u16v(buf: &[u8], b: &Bx, at: usize) -> u16 {
        u16::from_be_bytes(buf[b.start + at..b.start + at + 2].try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::boxparse as bp;
    use super::*;

    fn test_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("laps-mp4-test-{}-{name}.mp4", std::process::id()))
    }

    fn fake_frames(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                // Псевдо-кадр: маркерные байты + переменная длина.
                let mut f = vec![0xFF, 0xD8, 0xAA];
                f.extend_from_slice(&(i as u32).to_be_bytes());
                f.extend(std::iter::repeat_n(0xAB, i % 7));
                f.extend_from_slice(&[0xFF, 0xD9]);
                f
            })
            .collect()
    }

    /// Все кадры из mdat через таблицы stsz/co64: побайтово и по порядку.
    fn frames_via_index(buf: &[u8], stbl: &[bp::Bx]) -> Vec<Vec<u8>> {
        let stsz = bp::find(stbl, b"stsz");
        let co64 = bp::find(stbl, b"co64");
        let n = bp::u32v(buf, &stsz, 8) as usize;
        assert_eq!(
            bp::u32v(buf, &co64, 4) as usize,
            n,
            "co64 count == stsz count"
        );
        (0..n)
            .map(|i| {
                let size = bp::u32v(buf, &stsz, 12 + 4 * i) as usize;
                let off = bp::u64v(buf, &co64, 8 + 8 * i) as usize;
                buf[off..off + size].to_vec()
            })
            .collect()
    }

    /// Таймкоды кадров: разворачиваем stts (count, delta) в кумулятивные
    /// времена. Длительность последнего сэмпла (повтор дельты) не входит.
    fn sample_timestamps(buf: &[u8], stbl: &[bp::Bx], n: usize) -> Vec<u64> {
        let stts = bp::find(stbl, b"stts");
        let entries = bp::u32v(buf, &stts, 4) as usize;
        let mut ts = Vec::with_capacity(n);
        let mut t = 0u64;
        for e in 0..entries {
            let count = bp::u32v(buf, &stts, 8 + 8 * e) as usize;
            let delta = bp::u32v(buf, &stts, 12 + 8 * e) as u64;
            for _ in 0..count {
                ts.push(t);
                t += delta;
            }
        }
        assert_eq!(ts.len(), n, "stts покрывает все кадры");
        ts
    }

    /// stbl из moov для проверок таблиц.
    fn stbl_of(buf: &[u8], moov: &bp::Bx) -> Vec<bp::Bx> {
        let trak = bp::child(buf, moov, b"trak");
        let mdia = bp::child(buf, &trak, b"mdia");
        let minf = bp::child(buf, &mdia, b"minf");
        let stbl = bp::child(buf, &minf, b"stbl");
        bp::parse(buf, stbl.start, stbl.start + stbl.len)
    }

    #[test]
    fn test_ftyp_mdat_moov_and_roundtrip() {
        let path = test_path("roundtrip");
        let frames = fake_frames(3);
        {
            let mut writer = Box::new(Mp4Writer::create(&path, 1920, 1080).unwrap());
            for (i, frame) in frames.iter().enumerate() {
                writer
                    .write_frame(frame, 1_000_000 + i as u64 * 33)
                    .unwrap();
            }
            writer.finalize().unwrap();
        }

        let buf = std::fs::read(&path).unwrap();
        let top = bp::parse(&buf, 0, buf.len());
        let ftyp = bp::find(&top, b"ftyp");
        let mdat = bp::find(&top, b"mdat");
        let moov = bp::find(&top, b"moov");

        // ftyp: major brand isom, minor 0x200, compat содержит isom.
        assert_eq!(&buf[ftyp.start..ftyp.start + 4], b"isom");
        assert_eq!(bp::u32v(&buf, ftyp, 4), 0x200);
        // compat brands — сырой массив 4-байтных значений, не боксы.
        let compat = &buf[ftyp.start + 8..ftyp.start + ftyp.len];
        assert!(compat.chunks_exact(4).any(|c| c == b"isom"));

        // mdat: largesize (8 байт до start) пропатчен в полный размер
        // бокса; сразу за mdat лежит moov.
        let largesize = u64::from_be_bytes(buf[mdat.start - 8..mdat.start].try_into().unwrap());
        assert_eq!(largesize as usize, mdat.len + 16);
        // start — начало payload: у следующего бокса оно на заголовок (8) дальше.
        assert_eq!(mdat.start + mdat.len + 8, moov.start);

        // mvhd: timescale 1000, длительность rel[2] + дельта = 99.
        let mvhd = bp::child(&buf, moov, b"mvhd");
        assert_eq!(bp::u32v(&buf, &mvhd, 12), TIMESCALE);
        assert_eq!(bp::u32v(&buf, &mvhd, 16), 99);

        // stsd: sample entry 'jpeg', размеры кадра.
        let stbl = stbl_of(&buf, moov);
        let stsd = bp::find(&stbl, b"stsd");
        let entries = bp::parse(&buf, stsd.start + 8, stsd.start + stsd.len);
        let jpeg = bp::find(&entries, b"jpeg");
        assert_eq!(bp::u16v(&buf, jpeg, 24), 1920);
        assert_eq!(bp::u16v(&buf, jpeg, 26), 1080);

        // stss: все кадры ключевые, явно перечислены.
        let stss = bp::find(&stbl, b"stss");
        assert_eq!(bp::u32v(&buf, &stss, 4), 3);
        for i in 0..3 {
            assert_eq!(bp::u32v(&buf, &stss, 8 + 4 * i), (i + 1) as u32);
        }

        // Кадры через индекс побайтово совпадают, таймкоды — от 0 с шагом 33.
        assert_eq!(frames_via_index(&buf, &stbl), frames);
        assert_eq!(sample_timestamps(&buf, &stbl, 3), vec![0, 33, 66]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_jittered_timestamps() {
        let path = test_path("jitter");
        // Джиттерные дельты: 33, 34, 33, 50.
        let rel = [0u64, 33, 67, 100, 150];
        let frames = fake_frames(rel.len());
        {
            let mut writer = Box::new(Mp4Writer::create(&path, 640, 480).unwrap());
            for (i, frame) in frames.iter().enumerate() {
                writer.write_frame(frame, 5_000_000 + rel[i]).unwrap();
            }
            writer.finalize().unwrap();
        }

        let buf = std::fs::read(&path).unwrap();
        let top = bp::parse(&buf, 0, buf.len());
        let moov = bp::find(&top, b"moov");
        let stbl = stbl_of(&buf, moov);

        // Развёрнутые таймкоды совпадают с относительными ts кадров.
        assert_eq!(sample_timestamps(&buf, &stbl, rel.len()), rel.to_vec());

        // Длительность: 150 + последняя дельта 50 = 200.
        let mvhd = bp::child(&buf, moov, b"mvhd");
        assert_eq!(bp::u32v(&buf, &mvhd, 16), 200);

        assert_eq!(frames_via_index(&buf, &stbl), frames);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_empty_file() {
        let path = test_path("empty");
        Box::new(Mp4Writer::create(&path, 640, 480).unwrap())
            .finalize()
            .unwrap();

        let buf = std::fs::read(&path).unwrap();
        let top = bp::parse(&buf, 0, buf.len());
        let mdat = bp::find(&top, b"mdat");
        let moov = bp::find(&top, b"moov");
        let stbl = stbl_of(&buf, moov);

        // mdat пустой: largesize = 16 (только заголовок).
        assert_eq!(mdat.len, 0);
        let largesize = u64::from_be_bytes(buf[mdat.start - 8..mdat.start].try_into().unwrap());
        assert_eq!(largesize, 16);

        // Все таблицы пустые, длительность 0.
        assert_eq!(bp::u32v(&buf, bp::find(&stbl, b"stts"), 4), 0);
        assert_eq!(bp::u32v(&buf, bp::find(&stbl, b"stsz"), 8), 0);
        assert_eq!(bp::u32v(&buf, bp::find(&stbl, b"co64"), 4), 0);
        assert_eq!(bp::u32v(&buf, bp::find(&stbl, b"stss"), 4), 0);
        let mvhd = bp::child(&buf, moov, b"mvhd");
        assert_eq!(bp::u32v(&buf, &mvhd, 16), 0);

        std::fs::remove_file(&path).ok();
    }
}
