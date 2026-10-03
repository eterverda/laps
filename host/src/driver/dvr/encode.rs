//! Перекодирование RGBA → jpeg-кадр для записи YUYV-потока.
//! jpeg-камеры сюда не попадают: у них кадры идут в контейнер как есть
//! (passthrough, см. pipeline в driver/camera).

use std::io;

#[cfg(not(any(feature = "jpeg-rusturbo", feature = "jpeg-encoder")))]
compile_error!("Enable feature `jpeg-rusturbo` (default) or `jpeg-encoder`");

#[cfg(all(feature = "jpeg-rusturbo", feature = "jpeg-encoder"))]
compile_error!("features `jpeg-rusturbo` and `jpeg-encoder` are mutually exclusive");

/// Кодек выбирается cargo-фичей: `jpeg-rusturbo` (default) или
/// `jpeg-encoder`.
#[cfg(feature = "jpeg-rusturbo")]
pub fn encode_rgba_to_jpeg(rgba: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    const QUALITY: u8 = 60;
    let mut buf = Vec::new();
    let mut enc = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut buf, QUALITY);
    enc.encode_rgba(rgba, width, height)
        .map_err(|e| io::Error::other(format!("jpeg-rusturbo: {e}")))?;
    Ok(buf)
}

#[cfg(feature = "jpeg-encoder")]
pub fn encode_rgba_to_jpeg(rgba: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    const QUALITY: u8 = 60;
    let mut buf = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut buf, QUALITY);
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

    /// Раунд-трип: encode_rgba_to_jpeg → zune-decode (тот же декодер,
    /// что крутит live view). Независим от выбранного cargo-фичей кодека.
    #[test]
    fn output_decodes_by_zune() {
        let (w, h) = (1920u32, 1080u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
            px[0] = (i % 251) as u8;
            px[1] = (i % 241) as u8;
            px[2] = (i % 239) as u8;
            px[3] = 255;
        }
        let out = encode_rgba_to_jpeg(&rgba, w, h).unwrap();
        assert!(out.starts_with(&[0xFF, 0xD8]) && out.ends_with(&[0xFF, 0xD9]));

        let mut decoder =
            zune_jpeg::JpegDecoder::new(zune_jpeg::zune_core::bytestream::ZCursor::new(out));
        let pixels = decoder.decode().unwrap();
        assert_eq!(pixels.len(), (w * h * 3) as usize);
    }
}
