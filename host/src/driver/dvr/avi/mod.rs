//! AVI muxer (RIFF) для одного видеопотока. Всё little-endian.
//! Писатель агностичен к кодеку:
//! кадры — opaque чанки "00dc", fourcc потока задаётся в create().
//! Индекс помечает все кадры ключевыми — корректно только для intra-only
//! кодеков (MJPEG и т.п.).

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

const AVIF_HASINDEX: u32 = 0x10;
const AVIIF_KEYFRAME: u32 = 0x10;

/// RIFF size-поля — u32, больше 4 ГиБ файл не описать. Пишем стоп заранее:
/// запас покрывает хвост idx1 (16 байт/кадр, ~37 ч записи при 30 fps).
const MAX_FILE_SIZE: u64 = u32::MAX as u64 + 8 - 64 * 1024 * 1024;

/// Пишет AVI: заголовки с нулевыми патч-полями, кадры подряд, idx1 в конце.
/// Патчи (количество кадров, макс. размер, размеры списков) — в finalize().
pub struct AviWriter {
    writer: BufWriter<File>,
    avih_pos: u64,
    strh_pos: u64,
    movi_size_pos: u64,
    movi_data_start: u64,
    frames: u32,
    max_frame_size: u32,
    index: Vec<(u32, u32)>,
}

impl AviWriter {
    /// `fourcc` — кодек потока ('MJPG', 'H264', ...), пишется в strh и strf.
    pub fn create(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        fourcc: [u8; 4],
    ) -> io::Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut writer = BufWriter::new(file);

        writer.write_all(b"RIFF")?;
        writer.write_all(&0u32.to_le_bytes())?; // RIFF size, патч в finalize
        writer.write_all(b"AVI ")?;

        // LIST 'hdrl': avih + LIST 'strl' (strh + strf). Патч-поля — нули.
        let mut hdrl = Vec::with_capacity(192);
        hdrl.extend_from_slice(b"hdrl");

        // avih, 56 байт данных.
        let avih_pos = 12 + 8 + hdrl.len() as u64 + 8;
        hdrl.extend_from_slice(b"avih");
        hdrl.extend_from_slice(&56u32.to_le_bytes());
        let avih = [
            1_000_000 / fps, // dwMicroSecPerFrame
            0,               // dwMaxBytesPerSec
            0,               // dwPaddingGranularity
            AVIF_HASINDEX,   // dwFlags
            0,               // dwTotalFrames (патч)
            0,               // dwInitialFrames
            1,               // dwStreams
            0,               // dwSuggestedBufferSize (патч)
            width,
            height,
            0,
            0,
            0,
            0, // dwReserved[4]
        ];
        for v in avih {
            hdrl.extend_from_slice(&v.to_le_bytes());
        }

        // LIST 'strl': "strl" + strh chunk (64) + strf chunk (48).
        hdrl.extend_from_slice(b"LIST");
        hdrl.extend_from_slice(&116u32.to_le_bytes());
        hdrl.extend_from_slice(b"strl");

        // strh, 56 байт данных.
        let strh_pos = 12 + 8 + hdrl.len() as u64 + 8;
        hdrl.extend_from_slice(b"strh");
        hdrl.extend_from_slice(&56u32.to_le_bytes());
        let mut strh = Vec::with_capacity(56);
        strh.extend_from_slice(b"vids");
        strh.extend_from_slice(&fourcc); // fccHandler
        strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
        strh.extend_from_slice(&0u32.to_le_bytes()); // wPriority + wLanguage
        strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
        strh.extend_from_slice(&(1_000_000 / fps).to_le_bytes()); // dwScale
        strh.extend_from_slice(&1_000_000u32.to_le_bytes()); // dwRate
        strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
        strh.extend_from_slice(&0u32.to_le_bytes()); // dwLength (патч)
        strh.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize (патч)
        strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwQuality
        strh.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
        for v in [0i16, 0i16, width as i16, height as i16] {
            strh.extend_from_slice(&v.to_le_bytes()); // rcFrame
        }
        debug_assert_eq!(strh.len(), 56);
        hdrl.extend_from_slice(&strh);

        // strf = BITMAPINFOHEADER, 40 байт. biSizeImage = 0 (не патчим).
        let mut strf = Vec::with_capacity(40);
        strf.extend_from_slice(&40u32.to_le_bytes()); // biSize
        strf.extend_from_slice(&(width as i32).to_le_bytes());
        strf.extend_from_slice(&(height as i32).to_le_bytes()); // положительная
        strf.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        strf.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
        strf.extend_from_slice(&fourcc); // biCompression
        strf.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
        strf.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
        strf.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
        strf.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
        strf.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
        debug_assert_eq!(strf.len(), 40);
        hdrl.extend_from_slice(b"strf");
        hdrl.extend_from_slice(&(strf.len() as u32).to_le_bytes());
        hdrl.extend_from_slice(&strf);

        writer.write_all(b"LIST")?;
        writer.write_all(&(hdrl.len() as u32).to_le_bytes())?;
        writer.write_all(&hdrl)?;

        // LIST 'movi': заголовок, данные кадров пишутся write_frame.
        let list_pos = writer.stream_position()?;
        writer.write_all(b"LIST")?;
        writer.write_all(&0u32.to_le_bytes())?; // LIST size, патч в finalize
        writer.write_all(b"movi")?;
        let movi_size_pos = list_pos + 4;
        let movi_data_start = writer.stream_position()?;

        Ok(Self {
            writer,
            avih_pos,
            strh_pos,
            movi_size_pos,
            movi_data_start,
            frames: 0,
            max_frame_size: 0,
            index: Vec::new(),
        })
    }

    /// Пишет один кадр: "00dc" + данные + pad до чётной границы.
    /// Ошибка FileTooLarge при приближении к 4 ГиБ — writer обязан
    /// прекратить запись, иначе RIFF size-поля переполнятся молча.
    pub fn write_frame(&mut self, data: &[u8]) -> io::Result<()> {
        let pos = self.writer.stream_position()?;
        let chunk_len = 8 + data.len() as u64 + (data.len() as u64 % 2);
        if pos + chunk_len > MAX_FILE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "AVI exceeds 4 GiB RIFF limit, recording must stop",
            ));
        }
        let offset = (pos - self.movi_data_start) as u32;
        let len = data.len() as u32;
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(b"00dc");
        header[4..].copy_from_slice(&len.to_le_bytes());
        self.writer.write_all(&header)?;
        self.writer.write_all(data)?;
        if len % 2 == 1 {
            self.writer.write_all(&[0])?; // pad не входит в len
        }
        self.index.push((offset, len));
        self.frames += 1;
        self.max_frame_size = self.max_frame_size.max(len);
        Ok(())
    }

    /// Периодический flush + sync для живучести к крашу.
    pub fn sync_data(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    /// Дописывает idx1, патчит RIFF/avih/strh/movi-размеры, flush + sync_all.
    pub fn finalize(mut self) -> io::Result<()> {
        let n = self.index.len() as u32;
        let mut idx1 = Vec::with_capacity(8 + 16 * n as usize);
        idx1.extend_from_slice(b"idx1");
        idx1.extend_from_slice(&(16 * n).to_le_bytes());
        for &(offset, len) in &self.index {
            idx1.extend_from_slice(b"00dc");
            idx1.extend_from_slice(&AVIIF_KEYFRAME.to_le_bytes());
            idx1.extend_from_slice(&offset.to_le_bytes());
            idx1.extend_from_slice(&len.to_le_bytes());
        }
        self.writer.write_all(&idx1)?;
        self.writer.flush()?;

        let file = self.writer.get_ref();
        let file_size = file.metadata()?.len();
        if file_size - 8 > u32::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "AVI exceeds 4 GiB RIFF limit",
            ));
        }
        let idx1_pos = file_size - idx1.len() as u64;

        file.write_all_at(&((file_size - 8) as u32).to_le_bytes(), 4)?; // RIFF size
        file.write_all_at(&self.frames.to_le_bytes(), self.avih_pos + 16)?; // dwTotalFrames
        file.write_all_at(&self.max_frame_size.to_le_bytes(), self.avih_pos + 28)?; // dwSuggestedBufferSize
        file.write_all_at(&self.frames.to_le_bytes(), self.strh_pos + 32)?; // dwLength
        file.write_all_at(&self.max_frame_size.to_le_bytes(), self.strh_pos + 36)?; // dwSuggestedBufferSize
        let movi_size = (idx1_pos - (self.movi_size_pos + 4)) as u32;
        file.write_all_at(&movi_size.to_le_bytes(), self.movi_size_pos)?; // LIST movi size
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
