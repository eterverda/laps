//! Муксер и демуксер ISOBMFF (MP4/MOV) для одного MJPEG-видеопотока.
//! Общий движок семейства: MP4/MOV — одни и те же боксы, различаются
//! брендами ftyp (выбор — `IsobmffWriter::create_mp4/create_mov`); читает
//! `IsobmffReader::open` (для чтения бренды не важны).
//! Кадры — self-contained JPEG, все ключевые, stss пишем явно. Реальные
//! миллисекундные таймкоды (timescale 1000): длительность sample k —
//! это rel[k+1]-rel[k], последний повторяет предыдущую дельту, переменный
//! frame rate отражается честно, fps на входе не нужен. Индекс для seek'а —
//! co64+stsz+stts в moov (moov после mdat, ридер читает индекс за один
//! проход). Файл живой до finalize() (патч largesize mdat), живучесть к
//! крашу — через sync_data().

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::{VideoReader, VideoWriter};
use crate::config::camera::ResolutionConfig;
use std::io::Error as IoError;
use std::io::ErrorKind as IoErrorKind;
use std::io::Result as IoResult;

/// Таймлайн в миллисекундах: дельты таймкодов пишем без округления.
const TIMESCALE: u32 = 1000;

const BRAND_ISOM: [u8; 4] = *b"isom";
const BRAND_ISO2: [u8; 4] = *b"iso2";
const BRAND_MP41: [u8; 4] = *b"mp41";
const BRAND_QT: [u8; 4] = *b"qt  ";

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

/// Писатель ISOBMFF (MP4/MOV): единый тип семейства, различие — только
/// бренды ftyp. `ts_ms` — момент кадра, мс с Unix-эпохи; внутри файла —
/// относительно первого кадра.
pub struct IsobmffWriter {
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

impl IsobmffWriter {
    pub fn create_mp4(path: &Path, resolution: ResolutionConfig) -> IoResult<Self> {
        // major brand дублируется в compat — так принято у MP4.
        Self::create(
            path,
            resolution,
            BRAND_ISOM,
            &[BRAND_ISOM, BRAND_ISO2, BRAND_MP41],
        )
    }

    pub fn create_mov(path: &Path, resolution: ResolutionConfig) -> IoResult<Self> {
        Self::create(path, resolution, BRAND_QT, &[BRAND_QT])
    }

    /// MP4 и MOV различаются только брендами ftyp — они и есть «flavor».
    fn create(
        path: &Path,
        resolution: ResolutionConfig,
        major_brand: [u8; 4],
        compat_brands: &[[u8; 4]],
    ) -> IoResult<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut writer = BufWriter::new(file);

        // ftyp.
        let payload_len = 8 + 4 * compat_brands.len();
        writer.write_all(&be32(payload_len as u32 + 8))?;
        writer.write_all(b"ftyp")?;
        writer.write_all(&major_brand)?;
        writer.write_all(&be32(0x200))?; // minor version
        for brand in compat_brands {
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
            width: resolution.width,
            height: resolution.height,
            first_ts: None,
            rel_ts: Vec::new(),
            sizes: Vec::new(),
            offsets: Vec::new(),
        })
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
        // colr (nclx): BT.601, full range. JPEG-кадры у нас full-range
        // JFIF; без декларации плееры угадывают studio swing для видео
        // и растягивают mid-tones. Идёт после полей entry, codec-specific
        // боксов у 'jpeg' нет.
        let mut colr = Vec::new();
        colr.extend_from_slice(b"nclx");
        colr.extend_from_slice(&be16(6)); // colour_primaries: BT.601
        colr.extend_from_slice(&be16(6)); // transfer_characteristics: BT.601
        colr.extend_from_slice(&be16(6)); // matrix_coefficients: BT.601
        colr.push(0x80); // full_range_flag = 1
        push_box(&mut entry, b"colr", &colr);
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

impl VideoWriter for IsobmffWriter {
    /// Пишет кадр в mdat; ts_ms — момент кадра, мс с Unix-эпохи
    /// (внутри — относительно первого кадра).
    fn write_frame(&mut self, data: &[u8], ts_ms: u64) -> IoResult<()> {
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
    fn sync_data(&mut self) -> IoResult<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    /// Патчит largesize mdat, пишет moov (весь индекс — в памяти), flush +
    /// sync_all.
    fn finalize(mut self: Box<Self>) -> IoResult<()> {
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
}

/// Ридер ISOBMFF (MP4/MOV): индекс из moov при open, кадры — seek+read
/// в буфер вызывающего (как `MkvReader`). Поддерживает только наши файлы:
/// один видеотрек ('jpeg'), stsc «1 сэмпл на чанк» (sample i = chunk i),
/// stco или co64.
pub struct IsobmffReader {
    file: File,
    pts: Vec<u64>,
    offsets: Vec<u64>,
    sizes: Vec<u32>,
    max_frame: usize,
    resolution: ResolutionConfig,
}

impl IsobmffReader {
    /// Открыть файл: moov в память (таблицы — килобайты), индекс из
    /// stts/stsz/stco|co64.
    pub fn open(path: &Path) -> IoResult<Self> {
        let mut file = File::open(path)?;
        // Топ-уровень: ftyp, mdat, moov. Пейлоады не-моov пропускаем
        // seek'ом, moov забираем целиком.
        let mut moov: Option<Vec<u8>> = None;
        loop {
            let mut hdr = [0u8; 8];
            match file.read_exact(&mut hdr) {
                Ok(()) => {}
                Err(e) if e.kind() == IoErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let size32 = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
            let typ: [u8; 4] = hdr[4..8].try_into().unwrap();
            let pos = file.stream_position()?;
            let (payload_start, payload_len, to_eof) = if size32 == 1 {
                let mut ext = [0u8; 8];
                file.read_exact(&mut ext)?;
                let total = u64::from_be_bytes(ext);
                (file.stream_position()?, total.saturating_sub(16), false)
            } else if size32 == 0 {
                // Бокс до конца файла.
                (pos, file.metadata()?.len().saturating_sub(pos), true)
            } else {
                (pos, size32 as u64 - 8, false)
            };
            if &typ == b"moov" {
                let len = usize::try_from(payload_len)
                    .map_err(|_| invalid("moov larger than addressable memory"))?;
                let mut buf = vec![0u8; len];
                file.seek(SeekFrom::Start(payload_start))?;
                file.read_exact(&mut buf)?;
                moov = Some(buf);
            } else {
                file.seek(SeekFrom::Start(payload_start + payload_len))?;
            }
            if to_eof {
                break;
            }
        }
        let moov = moov.ok_or_else(|| invalid("no moov"))?;
        let index = Self::parse_moov(&moov)?;
        if index.pts.is_empty() {
            return Err(invalid("no frames"));
        }
        let max_frame = index.sizes.iter().max().copied().unwrap_or(0) as usize;
        Ok(Self {
            file,
            pts: index.pts,
            offsets: index.offsets,
            sizes: index.sizes,
            max_frame,
            resolution: index.resolution,
        })
    }

    /// moov → таймлайн и таблицы. Только version-0 боксы (так пишем мы);
    /// чужую разметку честно отвергаем, а не гадаем.
    fn parse_moov(moov: &[u8]) -> IoResult<TrackIndex> {
        let top = child_boxes(moov)?;
        let (_, trak_start, trak_len) = find_box(&top, b"trak")?;
        let trak = &moov[trak_start..trak_start + trak_len];

        let mdia = child_boxes(trak).and_then(|b| {
            let (_, s, l) = find_box(&b, b"mdia")?;
            Ok(trak[s..s + l].to_vec())
        })?;
        // hdlr: это видеотрек? (handler_type == 'vide').
        let mdia_boxes = child_boxes(&mdia)?;
        let (_, hdlr_s, hdlr_l) = find_box(&mdia_boxes, b"hdlr")?;
        if hdlr_l < 12 || &mdia[hdlr_s + 8..hdlr_s + 12] != b"vide" {
            return Err(invalid("first track is not video"));
        }
        // mdhd: timescale для перевода таймлайна в миллисекунды.
        let (_, mdhd_s, mdhd_l) = find_box(&mdia_boxes, b"mdhd")?;
        let mdhd = &mdia[mdhd_s..mdhd_s + mdhd_l];
        if mdhd.first().copied().unwrap_or(1) != 0 {
            return Err(invalid("mdhd version != 0"));
        }
        let timescale = ru32(mdhd, 12)? as u64;
        if timescale == 0 {
            return Err(invalid("mdhd timescale is 0"));
        }

        let (_, minf_s, minf_l) = find_box(&mdia_boxes, b"minf")?;
        let minf_boxes = child_boxes(&mdia[minf_s..minf_s + minf_l])?;
        let (_, stbl_s, stbl_l) = find_box(&minf_boxes, b"stbl")?;
        let stbl = &mdia[minf_s + stbl_s..minf_s + stbl_s + stbl_l];
        let stbl_boxes = child_boxes(stbl)?;

        // stsd: формат кадра и размеры. Берём только 'jpeg' (Photo-JPEG).
        let (_, stsd_s, stsd_l) = find_box(&stbl_boxes, b"stsd")?;
        let stsd = &stbl[stsd_s..stsd_s + stsd_l];
        if ru32(stsd, 4)? != 1 {
            return Err(invalid("stsd entry_count != 1"));
        }
        // Sample entry: box (size+format) + поля; width/height — u16
        // на смещениях 24/26 от начала payload entry.
        let entry_size = ru32(stsd, 8)? as usize;
        if entry_size < 8 + 28 || 8 + entry_size > stsd.len() {
            return Err(invalid("bad stsd sample entry"));
        }
        if &stsd[12..16] != b"jpeg" {
            return Err(invalid("unsupported codec (not 'jpeg')"));
        }
        let resolution = ResolutionConfig {
            width: ru16(stsd, 8 + 8 + 24)? as u32,
            height: ru16(stsd, 8 + 8 + 26)? as u32,
        };

        // stsz: размеры кадров.
        let (_, stsz_s, stsz_l) = find_box(&stbl_boxes, b"stsz")?;
        let stsz = &stbl[stsz_s..stsz_s + stsz_l];
        let sample_size = ru32(stsz, 4)?;
        let n = ru32(stsz, 8)? as usize;
        let sizes: Vec<u32> = if sample_size != 0 {
            vec![sample_size; n]
        } else {
            if stsz_l < 12 + 4 * n {
                return Err(invalid("truncated stsz"));
            }
            (0..n)
                .map(|i| ru32(stsz, 12 + 4 * i))
                .collect::<IoResult<_>>()?
        };

        // stts: (count, delta) → кумулятивные таймкоды, переводим в мс.
        let (_, stts_s, stts_l) = find_box(&stbl_boxes, b"stts")?;
        let stts = &stbl[stts_s..stts_s + stts_l];
        let entries = ru32(stts, 4)? as usize;
        if stts_l < 8 + 8 * entries {
            return Err(invalid("truncated stts"));
        }
        let mut pts = Vec::with_capacity(n);
        let mut t = 0u64;
        for e in 0..entries {
            let count = ru32(stts, 8 + 8 * e)? as u64;
            let delta = ru32(stts, 12 + 8 * e)? as u64;
            for _ in 0..count {
                if pts.len() >= n {
                    return Err(invalid("stts covers more samples than stsz"));
                }
                pts.push((t * 1000 + timescale / 2) / timescale);
                t += delta;
            }
        }
        if pts.len() != n {
            return Err(invalid("stts covers fewer samples than stsz"));
        }

        // stsc: принимаем только «1 сэмпл на чанк» — sample i = chunk i.
        let (_, stsc_s, stsc_l) = find_box(&stbl_boxes, b"stsc")?;
        let stsc = &stbl[stsc_s..stsc_s + stsc_l];
        if ru32(stsc, 4)? != 1 || ru32(stsc, 12)? != 1 {
            return Err(invalid("unsupported stsc layout (need 1 sample per chunk)"));
        }

        // stco или co64: оффсеты чанков == оффсеты кадров.
        let offsets: Vec<u64> = if let Ok((_, s, l)) = find_box(&stbl_boxes, b"co64") {
            let co64 = &stbl[s..s + l];
            if ru32(co64, 4)? as usize != n || l < 8 + 8 * n {
                return Err(invalid("co64 count mismatch"));
            }
            (0..n)
                .map(|i| ru64(co64, 8 + 8 * i))
                .collect::<IoResult<_>>()?
        } else {
            let (_, s, l) = find_box(&stbl_boxes, b"stco")?;
            let stco = &stbl[s..s + l];
            if ru32(stco, 4)? as usize != n || l < 8 + 4 * n {
                return Err(invalid("stco count mismatch"));
            }
            (0..n)
                .map(|i| Ok(ru32(stco, 8 + 4 * i)? as u64))
                .collect::<IoResult<_>>()?
        };

        Ok(TrackIndex {
            pts,
            offsets,
            sizes,
            resolution,
        })
    }
}

/// Индекс одного трека: результат разбора moov.
struct TrackIndex {
    pts: Vec<u64>,
    offsets: Vec<u64>,
    sizes: Vec<u32>,
    resolution: ResolutionConfig,
}

impl VideoReader for IsobmffReader {
    fn frame_count(&self) -> usize {
        self.pts.len()
    }

    fn timestamp(&self, i: usize) -> u64 {
        self.pts[i]
    }

    fn max_frame_len(&self) -> usize {
        self.max_frame
    }

    fn resolution(&self) -> ResolutionConfig {
        self.resolution
    }

    fn read_frame_into(&mut self, i: usize, buf: &mut [u8]) -> IoResult<(u64, usize)> {
        let (pts, offset, len) = (self.pts[i], self.offsets[i], self.sizes[i] as usize);
        if buf.len() < len {
            return Err(IoError::new(
                IoErrorKind::InvalidInput,
                "mp4: frame buffer too small",
            ));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut buf[..len])?;
        Ok((pts, len))
    }
}

fn invalid(msg: &str) -> IoError {
    IoError::new(IoErrorKind::InvalidData, format!("mp4: {msg}"))
}

/// Дочерние боксы payload'а: (тип, начало payload, длина payload).
/// Границы проверяются: битый файл — ошибка, не паника.
fn child_boxes(buf: &[u8]) -> IoResult<Vec<([u8; 4], usize, usize)>> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < buf.len() {
        if buf.len() - p < 8 {
            return Err(invalid("truncated box header"));
        }
        let size32 = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap());
        let typ: [u8; 4] = buf[p + 4..p + 8].try_into().unwrap();
        let (total, hdr) = if size32 == 1 {
            if buf.len() - p < 16 {
                return Err(invalid("truncated largesize header"));
            }
            (
                u64::from_be_bytes(buf[p + 8..p + 16].try_into().unwrap()),
                16usize,
            )
        } else if size32 == 0 {
            ((buf.len() - p) as u64, 8usize)
        } else {
            (size32 as u64, 8usize)
        };
        let total = usize::try_from(total).map_err(|_| invalid("box too large"))?;
        if total < hdr || total > buf.len() - p {
            return Err(invalid("bad box size"));
        }
        out.push((typ, p + hdr, total - hdr));
        p += total;
    }
    Ok(out)
}

fn find_box(boxes: &[([u8; 4], usize, usize)], typ: &[u8; 4]) -> IoResult<([u8; 4], usize, usize)> {
    boxes
        .iter()
        .find(|b| &b.0 == typ)
        .copied()
        .ok_or_else(|| invalid(&format!("no {} box", String::from_utf8_lossy(typ))))
}

/// BE-числа из payload с проверкой границ.
fn slice(buf: &[u8], at: usize, len: usize) -> IoResult<&[u8]> {
    buf.get(at..at + len)
        .ok_or_else(|| invalid("truncated box payload"))
}

fn ru32(buf: &[u8], at: usize) -> IoResult<u32> {
    Ok(u32::from_be_bytes(slice(buf, at, 4)?.try_into().unwrap()))
}

fn ru64(buf: &[u8], at: usize) -> IoResult<u64> {
    Ok(u64::from_be_bytes(slice(buf, at, 8)?.try_into().unwrap()))
}

fn ru16(buf: &[u8], at: usize) -> IoResult<u16> {
    Ok(u16::from_be_bytes(slice(buf, at, 2)?.try_into().unwrap()))
}

// --- ISOBMFF-примитивы (связаны с IsobmffWriter) ---
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

/// Мини-парсер боксов для тестов семейства (mp4.rs, mov.rs).
#[cfg(test)]
pub mod boxparse {
    #[derive(Clone)]
    pub struct Bx {
        pub typ: [u8; 4],
        pub start: usize, // начало payload
        pub len: usize,
    }

    /// Разобрать боксы уровня; largesize (size==1) поддержан — им пишется
    /// mdat.
    pub fn parse(buf: &[u8], from: usize, to: usize) -> Vec<Bx> {
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

    pub fn find<'a>(bx: &'a [Bx], typ: &[u8; 4]) -> &'a Bx {
        bx.iter()
            .find(|b| &b.typ == typ)
            .unwrap_or_else(|| panic!("box {:?} not found", String::from_utf8_lossy(typ)))
    }

    /// Первый дочерний бокс заданного типа, по значению — удобно в цепочках
    /// без промежуточных привязок.
    pub fn child(buf: &[u8], parent: &Bx, typ: &[u8; 4]) -> Bx {
        find(&parse(buf, parent.start, parent.start + parent.len), typ).clone()
    }

    /// u32 из payload бокса по смещению (payload начинается с start).
    pub fn u32v(buf: &[u8], b: &Bx, at: usize) -> u32 {
        u32::from_be_bytes(buf[b.start + at..b.start + at + 4].try_into().unwrap())
    }

    pub fn u64v(buf: &[u8], b: &Bx, at: usize) -> u64 {
        u64::from_be_bytes(buf[b.start + at..b.start + at + 8].try_into().unwrap())
    }

    pub fn u16v(buf: &[u8], b: &Bx, at: usize) -> u16 {
        u16::from_be_bytes(buf[b.start + at..b.start + at + 2].try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::boxparse as bp;
    use super::*;
    use std::env::temp_dir;
    use std::fs;
    use std::iter::repeat_n;
    use std::path::PathBuf;
    use std::process;

    fn test_path(name: &str) -> PathBuf {
        temp_dir().join(format!("laps-mp4-test-{}-{name}.mp4", process::id()))
    }

    fn fake_frames(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                // Псевдо-кадр: маркерные байты + переменная длина.
                let mut f = vec![0xFF, 0xD8, 0xAA];
                f.extend_from_slice(&(i as u32).to_be_bytes());
                f.extend(repeat_n(0xAB, i % 7));
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
            let mut writer = Box::new(
                IsobmffWriter::create_mp4(
                    &path,
                    ResolutionConfig {
                        width: 1920,
                        height: 1080,
                    },
                )
                .unwrap(),
            );
            for (i, frame) in frames.iter().enumerate() {
                writer
                    .write_frame(frame, 1_000_000 + i as u64 * 33)
                    .unwrap();
            }
            writer.finalize().unwrap();
        }

        let buf = fs::read(&path).unwrap();
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

        // stsd: sample entry 'jpeg', размеры кадра, colr — full range.
        let stbl = stbl_of(&buf, moov);
        let stsd = bp::find(&stbl, b"stsd");
        let entries = bp::parse(&buf, stsd.start + 8, stsd.start + stsd.len);
        let jpeg = bp::find(&entries, b"jpeg");
        assert_eq!(bp::u16v(&buf, jpeg, 24), 1920);
        assert_eq!(bp::u16v(&buf, jpeg, 26), 1080);
        // colr: после 78 байт полей sample entry (6+2+2+2+12+2+2+4+4+4+2+32+2+2).
        let colr_at = jpeg.start + 78;
        assert_eq!(&buf[colr_at + 4..colr_at + 8], b"colr");
        assert_eq!(
            u32::from_be_bytes(buf[colr_at..colr_at + 4].try_into().unwrap()),
            19
        );
        let p = colr_at + 8;
        assert_eq!(&buf[p..p + 4], b"nclx");
        assert_eq!(u16::from_be_bytes(buf[p + 4..p + 6].try_into().unwrap()), 6); // primaries
        assert_eq!(
            u16::from_be_bytes(buf[p + 8..p + 10].try_into().unwrap()),
            6
        ); // matrix
        assert_eq!(buf[p + 10] & 0x80, 0x80); // full range

        // stss: все кадры ключевые, явно перечислены.
        let stss = bp::find(&stbl, b"stss");
        assert_eq!(bp::u32v(&buf, &stss, 4), 3);
        for i in 0..3 {
            assert_eq!(bp::u32v(&buf, &stss, 8 + 4 * i), (i + 1) as u32);
        }

        // Кадры через индекс побайтово совпадают, таймкоды — от 0 с шагом 33.
        assert_eq!(frames_via_index(&buf, &stbl), frames);
        assert_eq!(sample_timestamps(&buf, &stbl, 3), vec![0, 33, 66]);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_jittered_timestamps() {
        let path = test_path("jitter");
        // Джиттерные дельты: 33, 34, 33, 50.
        let rel = [0u64, 33, 67, 100, 150];
        let frames = fake_frames(rel.len());
        {
            let mut writer = Box::new(
                IsobmffWriter::create_mp4(
                    &path,
                    ResolutionConfig {
                        width: 640,
                        height: 480,
                    },
                )
                .unwrap(),
            );
            for (i, frame) in frames.iter().enumerate() {
                writer.write_frame(frame, 5_000_000 + rel[i]).unwrap();
            }
            writer.finalize().unwrap();
        }

        let buf = fs::read(&path).unwrap();
        let top = bp::parse(&buf, 0, buf.len());
        let moov = bp::find(&top, b"moov");
        let stbl = stbl_of(&buf, moov);

        // Развёрнутые таймкоды совпадают с относительными ts кадров.
        assert_eq!(sample_timestamps(&buf, &stbl, rel.len()), rel.to_vec());

        // Длительность: 150 + последняя дельта 50 = 200.
        let mvhd = bp::child(&buf, moov, b"mvhd");
        assert_eq!(bp::u32v(&buf, &mvhd, 16), 200);

        assert_eq!(frames_via_index(&buf, &stbl), frames);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_empty_file() {
        let path = test_path("empty");
        Box::new(
            IsobmffWriter::create_mp4(
                &path,
                ResolutionConfig {
                    width: 640,
                    height: 480,
                },
            )
            .unwrap(),
        )
        .finalize()
        .unwrap();

        let buf = fs::read(&path).unwrap();
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

        fs::remove_file(&path).ok();
    }

    /// Ридер ISOBMFF: round-trip писатель→ридер — индекс, кадры побайтово,
    /// таймкоды, random access. Общая проверка для MP4 и MOV.
    fn reader_roundtrip(
        path: &Path,
        writer: Box<dyn VideoWriter>,
        frames: &[Vec<u8>],
        rel: &[u64],
        w: u32,
        h: u32,
    ) {
        use crate::driver::dvr::VideoReader as _;
        {
            let mut writer = writer;
            for (i, frame) in frames.iter().enumerate() {
                writer.write_frame(frame, 9_000_000 + rel[i]).unwrap();
            }
            writer.finalize().unwrap();
        }
        let mut reader = IsobmffReader::open(path).unwrap();
        assert_eq!(reader.frame_count(), frames.len());
        assert_eq!(reader.resolution().width, w);
        assert_eq!(reader.resolution().height, h);
        assert_eq!(
            reader.max_frame_len(),
            frames.iter().map(|f| f.len()).max().unwrap()
        );

        // Таймкоды: единицы timescale 1000 == миллисекундам.
        for (i, &t) in rel.iter().enumerate() {
            assert_eq!(reader.timestamp(i), t, "pts кадра {i}");
        }

        // Кадры побайтово, вперёд и вразнобую (random access).
        let mut buf = vec![0u8; reader.max_frame_len()];
        let mut order: Vec<usize> = (0..frames.len()).collect();
        order.reverse();
        for i in order {
            let (pts, len) = reader.read_frame_into(i, &mut buf).unwrap();
            assert_eq!(pts, rel[i]);
            assert_eq!(&buf[..len], frames[i].as_slice(), "кадр {i}");
        }

        // Маленький буфер — честная ошибка, не паника.
        let mut tiny = [0u8; 4];
        let err = reader.read_frame_into(0, &mut tiny).unwrap_err();
        assert_eq!(err.kind(), IoErrorKind::InvalidInput);
    }

    #[test]
    fn test_reader_roundtrip_mp4() {
        let path = test_path("reader-mp4");
        let rel = [0u64, 33, 67, 100, 150];
        let frames = fake_frames(rel.len());
        let writer = Box::new(
            IsobmffWriter::create_mp4(
                &path,
                ResolutionConfig {
                    width: 640,
                    height: 480,
                },
            )
            .unwrap(),
        );
        reader_roundtrip(&path, writer, &frames, &rel, 640, 480);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_reader_roundtrip_mov() {
        let path = temp_dir().join(format!("laps-mov-test-{}-reader.mov", process::id()));
        let rel = [0u64, 40, 80];
        let frames = fake_frames(rel.len());
        let writer = Box::new(
            IsobmffWriter::create_mov(
                &path,
                ResolutionConfig {
                    width: 1280,
                    height: 720,
                },
            )
            .unwrap(),
        );
        reader_roundtrip(&path, writer, &frames, &rel, 1280, 720);
        // Отличие MP4 — только бренды ftyp: у MOV major brand 'qt  '.
        let buf = fs::read(&path).unwrap();
        let top = bp::parse(&buf, 0, buf.len());
        let ftyp = bp::find(&top, b"ftyp");
        assert_eq!(&buf[ftyp.start..ftyp.start + 4], b"qt  ");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_reader_rejects_foreign_file() {
        // Не-ISOBMFF файл — ошибка, не паника.
        let path = test_path("reader-foreign");
        fs::write(&path, b"not a movie at all").unwrap();
        let err = IsobmffReader::open(&path).err().unwrap();
        assert_eq!(err.kind(), IoErrorKind::InvalidData);
        fs::remove_file(&path).ok();
    }
}
