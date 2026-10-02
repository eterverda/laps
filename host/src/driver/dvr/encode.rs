//! Перекодирование RGBA → jpeg-кадр для записи YUYV-потока.
//! jpeg-камеры сюда не попадают: у них кадры идут в контейнер как есть
//! (passthrough, см. webcam pipeline).

use std::io;

/// Кодирует RGBA-кадр в jpeg. Вызывается из writer-потока Recorder —
/// вне capture-потока, замер длительности — у вызывающего.
pub fn encode_rgba_to_jpeg(rgba: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    const JPEG_QUALITY: u8 = 60;

    let mut buf = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut buf, JPEG_QUALITY);
    encoder
        .encode(
            rgba,
            width as u16,
            height as u16,
            jpeg_encoder::ColorType::Rgba,
        )
        .map_err(|e| io::Error::other(format!("jpeg encode: {e}")))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::encode_rgba_to_jpeg;

    #[test]
    fn encodes_minimal_frame() {
        // 8x8 — минимальный блок JPEG; чёрный квадрат.
        let rgba = vec![0u8; 8 * 8 * 4];
        let jpeg = encode_rgba_to_jpeg(&rgba, 8, 8).unwrap();
        assert!(jpeg.starts_with(&[0xFF, 0xD8]), "no SOI marker");
        assert!(jpeg.ends_with(&[0xFF, 0xD9]), "no EOI marker");
    }
}
