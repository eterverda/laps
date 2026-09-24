//! MOV-муксер (QuickTime) для одного MJPEG-видеопотока. Тот же ISOBMFF,
//! что и MP4 (см. `mp4.rs`), отличие — бренды ftyp (`qt  `). Кодирование,
//! таймкоды, индекс и структура moov общие, меняем только flavor.

use std::io;
use std::path::Path;

use super::VideoWriter;
use super::mp4::{Flavor, IsobmffCore};

/// Писатель MOV: ftyp(qt  ) + mdat + moov.
pub struct MovWriter {
    core: IsobmffCore,
}

impl MovWriter {
    pub fn create(path: &Path, width: u32, height: u32) -> io::Result<Self> {
        Ok(Self {
            core: IsobmffCore::create(path, width, height, Flavor::Mov)?,
        })
    }
}

impl VideoWriter for MovWriter {
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

#[cfg(test)]
mod tests {
    use super::super::mp4::boxparse as bp;
    use super::*;

    fn test_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("laps-mov-test-{}-{name}.mov", std::process::id()))
    }

    fn fake_frames(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                let mut f = vec![0xFF, 0xD8, 0xAA];
                f.extend_from_slice(&(i as u32).to_be_bytes());
                f.extend_from_slice(&[0xFF, 0xD9]);
                f
            })
            .collect()
    }

    #[test]
    fn test_ftyp_qt_and_roundtrip() {
        let path = test_path("roundtrip");
        let frames = fake_frames(4);
        {
            let mut writer = Box::new(MovWriter::create(&path, 1280, 720).unwrap());
            for (i, frame) in frames.iter().enumerate() {
                writer
                    .write_frame(frame, 7_000_000 + i as u64 * 33)
                    .unwrap();
            }
            writer.finalize().unwrap();
        }

        let buf = std::fs::read(&path).unwrap();
        let top = bp::parse(&buf, 0, buf.len());
        let ftyp = bp::find(&top, b"ftyp");
        let moov = bp::find(&top, b"moov");

        // Отличие от MP4 — major brand qt  .
        assert_eq!(&buf[ftyp.start..ftyp.start + 4], b"qt  ");

        // Кадры через stsz/co64 побайтово совпадают, таймкоды — от 0.
        let trak = bp::child(&buf, moov, b"trak");
        let mdia = bp::child(&buf, &trak, b"mdia");
        let minf = bp::child(&buf, &mdia, b"minf");
        let stbl_el = bp::child(&buf, &minf, b"stbl");
        let stbl = bp::parse(&buf, stbl_el.start, stbl_el.start + stbl_el.len);

        let stsz = bp::find(&stbl, b"stsz");
        let co64 = bp::find(&stbl, b"co64");
        let n = bp::u32v(&buf, &stsz, 8) as usize;
        assert_eq!(n, frames.len());
        for (i, frame) in frames.iter().enumerate() {
            let size = bp::u32v(&buf, &stsz, 12 + 4 * i) as usize;
            let off = bp::u64v(&buf, &co64, 8 + 8 * i) as usize;
            assert_eq!(&buf[off..off + size], frame.as_slice());
        }

        // stts: 4 равные дельты 33 схлопнуты в одну запись.
        let stts = bp::find(&stbl, b"stts");
        assert_eq!(bp::u32v(&buf, &stts, 4), 1);
        assert_eq!(bp::u32v(&buf, &stts, 8), 4); // count
        assert_eq!(bp::u32v(&buf, &stts, 12), 33); // delta

        // stss: все кадры явно перечислены.
        let stss = bp::find(&stbl, b"stss");
        assert_eq!(bp::u32v(&buf, &stss, 4), 4);

        std::fs::remove_file(&path).ok();
    }
}
