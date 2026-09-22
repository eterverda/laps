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
        let mut writer = MkvWriter::create(&path, 1920, 1080).unwrap();
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
        let mut writer = MkvWriter::create(&path, 640, 480).unwrap();
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
    MkvWriter::create(&path, 640, 480)
        .unwrap()
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
