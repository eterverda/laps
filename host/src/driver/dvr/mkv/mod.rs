//! MKV-муксер (Matroska/EBML) для одного видеопотока.
//! Кадры — MJPEG в SimpleBlock (все ключевые), таймкоды — реальные,
//! миллисекундные (TimecodeScale = 1 мс), поэтому fps на входе не нужен:
//! переменный frame rate отражается честно. Лимита размера файла, вроде
//! 4 ГиБ у RIFF, нет. Как и avi: файл живой до finalize() (патчим
//! размеры Segment/Cluster, SeekHead и Duration), живучесть к крашу —
//! через sync_data().

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Кластеры режем по таймкодам: внутри кластера SimpleBlock хранит
/// относительный i16-таймкод, ширина винта ограничивает относительное
/// смещение ~32767 мс — запас 5 с ни к чему не обязывает.
const CLUSTER_SPAN_MS: u64 = 5_000;

/// Резерв под SeekHead в начале Segment: заполняем Void'ом, в finalize()
/// патчим реальный SeekHead + добиваем Void'ом до того же размера.
const SEEKHEAD_RESERVE: u64 = 128;

// --- EBML-примитивы -------------------------------------------------------

/// Ширина vint в байтах для значения v (минимально достаточная).
fn vint_width(v: u64) -> usize {
    let mut width = 1;
    while width < 8 && v >= (1u64 << (7 * width)) {
        width += 1;
    }
    width
}

/// Закодировать v как vint фиксированной ширины: маркер — старший бит
/// первого байта. Достаём младшие `width` байт из BE-представления.
fn vint(v: u64, width: usize) -> [u8; 8] {
    (v | (1u64 << (7 * width))).to_be_bytes()
}

/// Дописать ID элемента заданной длины (значащие байты u32 BE).
fn push_id(buf: &mut Vec<u8>, id: u32, id_len: usize) {
    buf.extend_from_slice(&id.to_be_bytes()[4 - id_len..]);
}

/// Unsigned-элемент в буфер: ID + size vint + значение в минимальной ширине.
fn push_u(buf: &mut Vec<u8>, id: u32, id_len: usize, v: u64) {
    push_id(buf, id, id_len);
    let w = vint_width(v); // байтовая ширина значения
    let sw = vint_width(w as u64); // size vint кодирует длину, не значение
    buf.extend_from_slice(&vint(w as u64, sw)[8 - sw..]);
    buf.extend_from_slice(&v.to_be_bytes()[8 - w..]);
}

/// ASCII-элемент в буфер.
fn push_text(buf: &mut Vec<u8>, id: u32, id_len: usize, s: &str) {
    push_id(buf, id, id_len);
    let w = vint_width(s.len() as u64);
    buf.extend_from_slice(&vint(s.len() as u64, w)[8 - w..]);
    buf.extend_from_slice(s.as_bytes());
}

/// Элемент с payload из буфера.
fn push_elem(buf: &mut Vec<u8>, id: u32, id_len: usize, payload: &[u8]) {
    push_id(buf, id, id_len);
    let w = vint_width(payload.len() as u64);
    buf.extend_from_slice(&vint(payload.len() as u64, w)[8 - w..]);
    buf.extend_from_slice(payload);
}

fn write_elem(w: &mut impl Write, id: u32, id_len: usize, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(id_len + 9 + payload.len());
    push_elem(&mut buf, id, id_len, payload);
    w.write_all(&buf)
}

/// Писатель Matroska: EBML-заголовок, Segment с резервным SeekHead,
/// Info/Tracks, кластеры по ходу write_frame, Cues + патчи в finalize().
pub struct MkvWriter {
    writer: BufWriter<File>,
    /// Позиция 8-байтного size-винта Segment (патч в finalize).
    segment_size_pos: u64,
    /// Начало данных Segment: все позиции в Cues/SeekHead — отсюда.
    segment_data_start: u64,
    /// Позиция резерва под SeekHead (патч в finalize).
    seekhead_pos: u64,
    /// Позиция f64 Duration внутри Info (патч в finalize).
    info_duration_pos: u64,
    /// Абсолютные позиции Info и Tracks — для SeekHead.
    info_pos: u64,
    tracks_pos: u64,
    /// Открытый кластер: позиция его 5-байтного size-винта и начало данных.
    cluster_size_pos: Option<u64>,
    cluster_data_start: u64,
    /// Таймкод открытого кластера, мс (абсолютный, от первого кадра).
    cluster_tc: u64,
    /// (таймкод кластера, абсолютная позиция элемента Cluster) — для Cues.
    clusters: Vec<(u64, u64)>,
    /// Таймкод первого кадра: все таймкоды пишем относительно него.
    first_frame_ts: Option<u64>,
    /// Максимальный относительный таймкод — станет Duration.
    duration_ms: u64,
}

impl MkvWriter {
    pub fn create(path: &Path, width: u32, height: u32) -> io::Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut writer = BufWriter::new(file);

        // EBML-заголовок.
        let mut hdr = Vec::new();
        push_u(&mut hdr, 0x4286, 2, 1); // EBMLVersion
        push_u(&mut hdr, 0x42F7, 2, 1); // EBMLReadVersion
        push_u(&mut hdr, 0x42F2, 2, 4); // EBMLMaxIDLength
        push_u(&mut hdr, 0x42F3, 2, 8); // EBMLMaxSizeLength
        push_text(&mut hdr, 0x4282, 2, "matroska"); // DocType
        push_u(&mut hdr, 0x4287, 2, 4); // DocTypeVersion
        push_u(&mut hdr, 0x4285, 2, 2); // DocTypeReadVersion
        write_elem(&mut writer, 0x1A45DFA3, 4, &hdr)?; // EBML

        // Segment: размер — 8-байтный резерв, патч в finalize.
        writer.write_all(&0x18538067u32.to_be_bytes())?;
        let segment_size_pos = writer.stream_position()?;
        writer.write_all(&vint(0, 8))?;
        let segment_data_start = writer.stream_position()?;

        // SeekHead: пока Void на весь резерв, патч в finalize.
        let seekhead_pos = writer.stream_position()?;
        let void_size = SEEKHEAD_RESERVE - 2; // 0xEC + 1-байтный size vint
        writer.write_all(&[0xEC, 0xFE])?; // Void, size 126
        writer.write_all(&vec![0u8; void_size as usize])?;

        // Info: TimecodeScale = 1 мс, Duration — резерв f64 под патч.
        let mut info = Vec::new();
        push_u(&mut info, 0x2AD7B1, 3, 1_000_000); // TimecodeScale, нс
        push_id(&mut info, 0x4489, 2); // Duration
        info.extend_from_slice(&vint(8, 1)[7..]); // size vint: 8
        let duration_rel = info.len();
        info.extend_from_slice(&0f64.to_be_bytes());
        push_text(&mut info, 0x4D80, 2, "laps"); // MuxingApp
        push_text(&mut info, 0x5741, 2, "laps"); // WritingApp
        let hdr_len = 4 + vint_width(info.len() as u64) as u64;
        let info_pos = writer.stream_position()?;
        write_elem(&mut writer, 0x1549A966, 4, &info)?; // Info
        let info_duration_pos = info_pos + hdr_len + duration_rel as u64;

        // Tracks: один видеотрек, CodecID V_MJPEG, размеры кадра.
        let mut video = Vec::new();
        push_u(&mut video, 0xB0, 1, width as u64); // PixelWidth
        push_u(&mut video, 0xBA, 1, height as u64); // PixelHeight
        let mut entry = Vec::new();
        push_u(&mut entry, 0xD7, 1, 1); // TrackNumber
        push_u(&mut entry, 0x73C5, 2, 1); // TrackUID
        push_u(&mut entry, 0x83, 1, 1); // TrackType: video
        push_text(&mut entry, 0x86, 1, "V_MJPEG"); // CodecID
        push_elem(&mut entry, 0xE0, 1, &video); // Video
        let mut tracks = Vec::new();
        push_elem(&mut tracks, 0xAE, 1, &entry); // TrackEntry
        let tracks_pos = writer.stream_position()?;
        write_elem(&mut writer, 0x1654AE6B, 4, &tracks)?; // Tracks

        Ok(Self {
            writer,
            segment_size_pos,
            segment_data_start,
            seekhead_pos,
            info_duration_pos,
            info_pos,
            tracks_pos,
            cluster_size_pos: None,
            cluster_data_start: 0,
            cluster_tc: 0,
            clusters: Vec::new(),
            first_frame_ts: None,
            duration_ms: 0,
        })
    }

    /// Пишет один кадр как SimpleBlock с таймкодом ts_ms (мс с Unix-эпохи;
    /// внутри — относительно первого кадра). Кластер открывается при первом
    /// кадре и переключается, когда относительный таймкод уходит за спан.
    pub fn write_frame(&mut self, data: &[u8], ts_ms: u64) -> io::Result<()> {
        let first = *self.first_frame_ts.get_or_insert(ts_ms);
        let rel = ts_ms.saturating_sub(first);
        self.duration_ms = self.duration_ms.max(rel);

        match self.cluster_size_pos {
            None => self.open_cluster(rel)?,
            Some(_) if rel.saturating_sub(self.cluster_tc) >= CLUSTER_SPAN_MS => {
                self.close_cluster()?;
                self.open_cluster(rel)?;
            }
            _ => {}
        }

        // SimpleBlock: track vint 0x81, i16 таймкод от кластера, флаги
        // 0x80 (keyframe, без lacing), данные. Для интра-кодека каждый
        // кадр ключевой — скраббинг по Cues не нужен чаще кластеров.
        let mut head = Vec::with_capacity(2 + 9 + 4);
        push_id(&mut head, 0xA3, 1);
        let size = 4 + data.len() as u64;
        let w = vint_width(size);
        head.extend_from_slice(&vint(size, w)[8 - w..]);
        head.push(0x81); // TrackNumber = 1
        head.extend_from_slice(&((rel - self.cluster_tc) as i16).to_be_bytes());
        head.push(0x80); // keyframe
        self.writer.write_all(&head)?;
        self.writer.write_all(data)?;
        Ok(())
    }

    /// Периодический flush + sync для живучести к крашу.
    pub fn sync_data(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    /// Закрывает кластер, пишет Cues, патчит Duration/SeekHead/Segment,
    /// flush + sync_all.
    pub fn finalize(mut self) -> io::Result<()> {
        self.close_cluster()?;

        // Cues: по CuePoint на кластер; позиции — от начала данных Segment.
        let mut cues = Vec::new();
        for &(tc, pos) in &self.clusters {
            let mut tp = Vec::new();
            push_u(&mut tp, 0xF7, 1, 1); // CueTrack
            push_u(&mut tp, 0xF1, 1, pos - self.segment_data_start); // CueClusterPosition
            let mut point = Vec::new();
            push_u(&mut point, 0xB3, 1, tc); // CueTime
            push_elem(&mut point, 0xB7, 1, &tp); // CueTrackPositions
            push_elem(&mut cues, 0xBB, 1, &point); // CuePoint
        }
        let cues_pos = self.writer.stream_position()?;
        write_elem(&mut self.writer, 0x1C53BB6B, 4, &cues)?; // Cues
        self.writer.flush()?;

        let file = self.writer.get_ref();
        let file_size = file.metadata()?.len();

        let seg_size = file_size - self.segment_data_start;
        file.write_all_at(&vint(seg_size, 8), self.segment_size_pos)?;

        // Duration в мс (TimecodeScale = 1 мс).
        file.write_all_at(
            &(self.duration_ms as f64).to_be_bytes(),
            self.info_duration_pos,
        )?;

        // SeekHead: позиции Info/Tracks/Cues от начала данных Segment.
        // Фиксированная 8-байтная ширина SeekPosition — детерминированный
        // размер; добиваем резерв Void'ом.
        let info_rel = self.info_pos - self.segment_data_start;
        let tracks_rel = self.tracks_pos - self.segment_data_start;
        let mut payload = Vec::new();
        for (seek_id, pos) in [
            (0x1549A966u32, info_rel),
            (0x1654AE6Bu32, tracks_rel),
            (0x1C53BB6Bu32, cues_pos - self.segment_data_start),
        ] {
            let mut entry = Vec::new();
            push_id(&mut entry, 0x53AB, 2); // SeekID
            entry.extend_from_slice(&vint(4, 1)[7..]);
            entry.extend_from_slice(&seek_id.to_be_bytes());
            push_id(&mut entry, 0x53AC, 2); // SeekPosition
            entry.extend_from_slice(&vint(8, 1)[7..]); // size vint: 8 байт
            entry.extend_from_slice(&pos.to_be_bytes());
            push_elem(&mut payload, 0x4DBB, 2, &entry); // Seek
        }
        let mut patch = Vec::with_capacity(SEEKHEAD_RESERVE as usize);
        push_elem(&mut patch, 0x114D9B74, 4, &payload); // SeekHead
        let pad = SEEKHEAD_RESERVE - patch.len() as u64;
        debug_assert!(pad >= 2, "SeekHead reserve overflowed");
        let pad = pad as usize;
        patch.push(0xEC);
        patch.push(0x80 | (pad - 2) as u8); // Void на остаток (pad-2 <= 126)
        patch.extend(std::iter::repeat_n(0u8, pad - 2));
        debug_assert_eq!(patch.len() as u64, SEEKHEAD_RESERVE);
        file.write_all_at(&patch, self.seekhead_pos)?;

        file.sync_all()?;
        Ok(())
    }
}

impl MkvWriter {
    /// Открывает кластер с абсолютным (от первого кадра) таймкодом rel.
    fn open_cluster(&mut self, rel: u64) -> io::Result<()> {
        let cluster_pos = self.writer.stream_position()?;
        self.writer.write_all(&0x1F43B675u32.to_be_bytes())?;
        let size_pos = self.writer.stream_position()?;
        // 5-байтный size-винт: до 32 ГиБ на кластер — спан 5 с при любом
        // битрейте MJPEG укладывается с огромным запасом.
        self.writer.write_all(&vint(0, 5)[3..])?;
        self.cluster_data_start = self.writer.stream_position()?;
        let mut tc = Vec::new();
        push_u(&mut tc, 0xE7, 1, rel); // Timecode
        self.writer.write_all(&tc)?;
        self.cluster_size_pos = Some(size_pos);
        self.cluster_tc = rel;
        self.clusters.push((rel, cluster_pos));
        Ok(())
    }

    /// Патчит size-винт открытого кластера (flush перед write_all_at).
    fn close_cluster(&mut self) -> io::Result<()> {
        if let Some(size_pos) = self.cluster_size_pos.take() {
            let size = self.writer.stream_position()? - self.cluster_data_start;
            self.writer.flush()?;
            self.writer
                .get_ref()
                .write_all_at(&vint(size, 5)[3..], size_pos)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
