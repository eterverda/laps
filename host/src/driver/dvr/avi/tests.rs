use super::*;

fn test_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("laps-avi-test-{}-{name}.avi", std::process::id()))
}

fn fake_frames() -> Vec<Vec<u8>> {
    (0..5u8)
        .map(|i| {
            // Псевдо-кадр: маркерные байты по краям + переменная длина
            // (чётные и нечётные размеры, чтобы ловить pad-баги).
            let mut f = vec![0xAA, 0xBB, 0xCC, 0xDD];
            f.extend_from_slice(&i.to_le_bytes());
            f.extend(std::iter::repeat_n(0xAB, (i * 3) as usize));
            f.extend_from_slice(&[0xEE, 0xFF]);
            f
        })
        .collect()
}

fn u32_at(buf: &[u8], pos: u64) -> u32 {
    u32::from_le_bytes(buf[pos as usize..pos as usize + 4].try_into().unwrap())
}

#[test]
fn test_patches_and_idx1() {
    let path = test_path("patches");
    let frames = fake_frames();
    {
        let mut writer = AviWriter::create(&path, 1920, 1080, 30, *b"TEST").unwrap();
        for frame in &frames {
            writer.write_frame(frame).unwrap();
        }
        writer.finalize().unwrap();
    }

    let buf = std::fs::read(&path).unwrap();
    let file_size = buf.len() as u64;

    // RIFF size = file_size - 8.
    assert_eq!(u32_at(&buf, 4) as u64, file_size - 8);
    assert_eq!(&buf[8..12], b"AVI ", "AVI fourcc");
    assert_eq!(&buf[220..224], b"movi", "movi fourcc");
    // fourcc потока: strh handler (112) и strf biCompression (188).
    assert_eq!(&buf[112..116], b"TEST");
    assert_eq!(&buf[188..192], b"TEST");
    // avih: microsec/frame, dwTotalFrames, размеры.
    let avih = 32; // данные avih всегда на offset 32 в нашем скелете
    assert_eq!(u32_at(&buf, avih), 33_333);
    assert_eq!(u32_at(&buf, avih + 16), frames.len() as u32);
    assert_eq!(u32_at(&buf, avih + 32), 1920);
    assert_eq!(u32_at(&buf, avih + 36), 1080);
    // max frame size.
    let max_frame = frames.iter().map(|f| f.len()).max().unwrap() as u32;
    assert_eq!(u32_at(&buf, avih + 28), max_frame);

    // idx1: найти с конца, проверить записи.
    let idx1_pos = file_size - (8 + 16 * frames.len()) as u64;
    assert_eq!(&buf[idx1_pos as usize..idx1_pos as usize + 4], b"idx1");
    assert_eq!(u32_at(&buf, idx1_pos + 4) as usize, 16 * frames.len());

    // Кадры читаются через индекс: каждый кадр на месте, побайтово.
    for (i, frame) in frames.iter().enumerate() {
        let entry = idx1_pos + 8 + 16 * i as u64;
        let offset = u32_at(&buf, entry + 8) as u64;
        let len = u32_at(&buf, entry + 12) as usize;
        let start = 224 + offset + 8; // movi data start в нашем скелете
        assert_eq!(&buf[start as usize..start as usize + len], frame.as_slice());
    }

    std::fs::remove_file(&path).ok();
}

#[test]
fn test_empty_file() {
    let path = test_path("empty");
    AviWriter::create(&path, 640, 480, 30, *b"TEST")
        .unwrap()
        .finalize()
        .unwrap();

    let buf = std::fs::read(&path).unwrap();
    assert_eq!(u32_at(&buf, 32 + 16), 0); // dwTotalFrames = 0
    assert_eq!(&buf[buf.len() - 8..buf.len() - 4], b"idx1"); // пустой idx1

    std::fs::remove_file(&path).ok();
}
