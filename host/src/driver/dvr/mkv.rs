//! MKV-муксер (Matroska/EBML) для одного видеопотока.
//! Кадры — MJPEG в SimpleBlock (все ключевые), таймкоды — реальные,
//! миллисекундные (TimecodeScale = 1 мс), поэтому fps на входе не нужен:
//! переменный frame rate отражается честно. Лимита размера файла, вроде
//! 4 ГиБ у RIFF, нет. Файл живой до finalize() (патчим размеры
//! Segment/Cluster, SeekHead и Duration), живучесть к крашу —
//! через sync_data().

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::VideoWriter;

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
}

impl VideoWriter for MkvWriter {
    /// Пишет один кадр как SimpleBlock с таймкодом ts_ms (мс с Unix-эпохи;
    /// внутри — относительно первого кадра). Кластер открывается при первом
    /// кадре и переключается, когда относительный таймкод уходит за спан.
    fn write_frame(&mut self, data: &[u8], ts_ms: u64) -> io::Result<()> {
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
    fn sync_data(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    /// Закрывает кластер, пишет Cues, патчит Duration/SeekHead/Segment,
    /// flush + sync_all.
    fn finalize(mut self: Box<Self>) -> io::Result<()> {
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
mod tests {
    use super::*;

    fn test_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("laps-mkv-test-{}-{name}.mkv", std::process::id()))
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

    // --- Мини-EBML-парсер для тестов ------------------------------------------

    struct El {
        id: u32,
        start: usize, // начало данных
        len: usize,   // длина данных
    }

    /// Длина vint по первому байту (позиция старшего установленного бита).
    fn vint_len(first: u8) -> usize {
        first.leading_zeros() as usize + 1
    }

    /// ID vint: значение включает маркерную единицу (BE всех байт).
    fn read_id(buf: &[u8], pos: usize) -> (u32, usize) {
        let len = vint_len(buf[pos]);
        let mut id = 0u32;
        for &b in &buf[pos..pos + len] {
            id = (id << 8) | b as u32;
        }
        (id, len)
    }

    /// Size vint: значение без маркерного бита.
    fn read_size(buf: &[u8], pos: usize) -> (u64, usize) {
        let len = vint_len(buf[pos]);
        let mut v = 0u64;
        for &b in &buf[pos..pos + len] {
            v = (v << 8) | b as u64;
        }
        (v & ((1u64 << (7 * len)) - 1), len)
    }

    /// Разобрать диапазон на элементы верхнего уровня.
    fn parse(buf: &[u8], from: usize, to: usize) -> Vec<El> {
        let mut out = Vec::new();
        let mut p = from;
        while p < to {
            let (id, id_len) = read_id(buf, p);
            let (size, size_len) = read_size(buf, p + id_len);
            let start = p + id_len + size_len;
            out.push(El {
                id,
                start,
                len: size as usize,
            });
            p = start + size as usize;
        }
        out
    }

    fn find<'a>(els: &'a [El], id: u32) -> &'a El {
        els.iter()
            .find(|e| e.id == id)
            .unwrap_or_else(|| panic!("element {id:#x} not found"))
    }

    fn u_value(buf: &[u8], el: &El) -> u64 {
        let mut v = 0u64;
        for &b in &buf[el.start..el.start + el.len] {
            v = (v << 8) | b as u64;
        }
        v
    }

    fn f64_value(buf: &[u8], el: &El) -> f64 {
        assert_eq!(el.len, 8);
        f64::from_be_bytes(buf[el.start..el.start + 8].try_into().unwrap())
    }

    /// Собрать все SimpleBlock'и из всех кластеров: (абс. таймкод, данные).
    fn simple_blocks(buf: &[u8], segment: &El) -> Vec<(i64, Vec<u8>)> {
        let children = parse(buf, segment.start, segment.start + segment.len);
        let mut out = Vec::new();
        for cluster in children.iter().filter(|e| e.id == 0x1F43B675) {
            let tc = u_value(
                buf,
                find(
                    &parse(buf, cluster.start, cluster.start + cluster.len),
                    0xE7,
                ),
            ) as i64;
            for block in parse(buf, cluster.start, cluster.start + cluster.len)
                .iter()
                .filter(|e| e.id == 0xA3)
            {
                let d = &buf[block.start..block.start + block.len];
                assert_eq!(d[0], 0x81, "track vint");
                let rel = i16::from_be_bytes(d[1..3].try_into().unwrap()) as i64;
                assert_eq!(d[3] & 0x80, 0x80, "keyframe flag");
                out.push((tc + rel, d[4..].to_vec()));
            }
        }
        out
    }

    // --- Тесты -----------------------------------------------------------------

    #[test]
    fn test_header_tracks_and_segment_size() {
        let path = test_path("header");
        let frames = fake_frames(3);
        {
            let mut writer = Box::new(MkvWriter::create(&path, 1920, 1080).unwrap());
            for (i, frame) in frames.iter().enumerate() {
                writer
                    .write_frame(frame, 1_000_000 + i as u64 * 33)
                    .unwrap();
            }
            writer.finalize().unwrap();
        }

        let buf = std::fs::read(&path).unwrap();
        let top = parse(&buf, 0, buf.len());
        let ebml = find(&top, 0x1A45DFA3);
        let segment = find(&top, 0x18538067);

        // DocType/версии из EBML-заголовка.
        let hdr_children = parse(&buf, ebml.start, ebml.start + ebml.len);
        assert_eq!(
            &buf[find(&hdr_children, 0x4282).start..find(&hdr_children, 0x4282).start + 8],
            b"matroska"
        );
        assert_eq!(u_value(&buf, find(&hdr_children, 0x4287)), 4); // DocTypeVersion

        // Segment size-винт (патченный) = реальный хвост файла.
        let seg_size_pos = ebml.start + ebml.len + 4; // за ID Segment
        let (seg_size, seg_size_len) = read_size(&buf, seg_size_pos);
        assert_eq!(seg_size as usize, buf.len() - segment.start);
        assert_eq!(seg_size_len, 8);

        // Tracks: CodecID и размеры.
        let seg_children = parse(&buf, segment.start, segment.start + segment.len);
        let tracks = find(&seg_children, 0x1654AE6B);
        let tracks_children = parse(&buf, tracks.start, tracks.start + tracks.len);
        let entry = find(&tracks_children, 0xAE);
        let entry_children = parse(&buf, entry.start, entry.start + entry.len);
        let codec = find(&entry_children, 0x86);
        assert_eq!(&buf[codec.start..codec.start + codec.len], b"V_MJPEG");
        let video = find(&entry_children, 0xE0);
        let video_children = parse(&buf, video.start, video.start + video.len);
        assert_eq!(u_value(&buf, find(&video_children, 0xB0)), 1920);
        assert_eq!(u_value(&buf, find(&video_children, 0xBA)), 1080);

        // Duration: от первого до последнего кадра, мс.
        let info = find(&seg_children, 0x1549A966);
        let info_children = parse(&buf, info.start, info.start + info.len);
        let last_ts = 1_000_000 + 2 * 33;
        assert_eq!(
            f64_value(&buf, find(&info_children, 0x4489)) as u64,
            last_ts - 1_000_000
        );

        // Кадры на месте, побайтово, таймкоды с 0 с шагом 33.
        let blocks = simple_blocks(&buf, segment);
        assert_eq!(blocks.len(), frames.len());
        for (i, ((tc, data), frame)) in blocks.iter().zip(&frames).enumerate() {
            assert_eq!(*tc, (i * 33) as i64);
            assert_eq!(data, frame);
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_clusters_cues_and_seekhead() {
        let path = test_path("clusters");
        let frames = fake_frames(400); // 400 * 33 мс ≈ 13 с — два переключения
        {
            let mut writer = Box::new(MkvWriter::create(&path, 640, 480).unwrap());
            for (i, frame) in frames.iter().enumerate() {
                writer
                    .write_frame(frame, 5_000_000 + i as u64 * 33)
                    .unwrap();
            }
            writer.finalize().unwrap();
        }

        let buf = std::fs::read(&path).unwrap();
        let top = parse(&buf, 0, buf.len());
        let segment = find(&top, 0x18538067);
        let seg_children = parse(&buf, segment.start, segment.start + segment.len);

        // Кластеры: ≥ 3 (спан 5 с, запись ~13 с).
        let cluster_els: Vec<&El> = seg_children.iter().filter(|e| e.id == 0x1F43B675).collect();
        assert!(cluster_els.len() >= 3, "expected >=3 clusters");

        // Cues: по CuePoint на кластер, позиции указывают на реальные
        // элементы Cluster (абсолютная позиция = Segment data start + rel).
        let cues = find(&seg_children, 0x1C53BB6B);
        let points = parse(&buf, cues.start, cues.start + cues.len);
        assert_eq!(points.len(), cluster_els.len());
        for (point, cluster) in points.iter().zip(&cluster_els) {
            let pc = parse(&buf, point.start, point.start + point.len);
            let tp = find(&pc, 0xB7);
            let tp_children = parse(&buf, tp.start, tp.start + tp.len);
            let rel = u_value(&buf, find(&tp_children, 0xF1)) as usize;
            let abs = segment.start + rel;
            let (id, _) = read_id(&buf, abs);
            assert_eq!(id, 0x1F43B675, "CueClusterPosition must point at Cluster");
            // Таймкод CuePoint == таймкоду кластера.
            let cluster_tc = u_value(
                &buf,
                find(
                    &parse(&buf, cluster.start, cluster.start + cluster.len),
                    0xE7,
                ),
            );
            assert_eq!(u_value(&buf, find(&pc, 0xB3)), cluster_tc);
        }

        // SeekHead: три записи, позиции указывают на Info/Tracks/Cues.
        let seekhead = find(&seg_children, 0x114D9B74);
        let seeks = parse(&buf, seekhead.start, seekhead.start + seekhead.len);
        assert_eq!(seeks.len(), 3);
        for seek in seeks {
            let sc = parse(&buf, seek.start, seek.start + seek.len);
            let seek_id = u_value(&buf, find(&sc, 0x53AB));
            let pos = u_value(&buf, find(&sc, 0x53AC)) as usize;
            let (id_at_pos, _) = read_id(&buf, segment.start + pos);
            assert_eq!(
                id_at_pos, seek_id as u32,
                "SeekHead position must point at the sought element"
            );
        }

        // Все кадры читаются, таймкоды монотонны от 0.
        let blocks = simple_blocks(&buf, segment);
        assert_eq!(blocks.len(), frames.len());
        for (i, (tc, _)) in blocks.iter().enumerate() {
            assert_eq!(*tc, (i * 33) as i64);
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_empty_file() {
        let path = test_path("empty");
        Box::new(MkvWriter::create(&path, 640, 480).unwrap())
            .finalize()
            .unwrap();

        let buf = std::fs::read(&path).unwrap();
        let top = parse(&buf, 0, buf.len());
        let segment = find(&top, 0x18538067);
        let seg_children = parse(&buf, segment.start, segment.start + segment.len);

        // Ни кластеров, ни CuePoint'ов; Duration = 0.
        assert!(seg_children.iter().all(|e| e.id != 0x1F43B675));
        let cues = find(&seg_children, 0x1C53BB6B);
        assert_eq!(cues.len, 0);
        let info = find(&seg_children, 0x1549A966);
        let info_children = parse(&buf, info.start, info.start + info.len);
        assert_eq!(f64_value(&buf, find(&info_children, 0x4489)), 0.0);

        std::fs::remove_file(&path).ok();
    }
}
