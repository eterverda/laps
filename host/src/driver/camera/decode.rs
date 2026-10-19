/// Кадр не декодирован — пропускаем его и живём дальше. Не сбой потока:
/// причины разнообразны и важны для диагностики, поэтому типизированы.
use crate::config::camera::PixelConfig;

#[derive(Debug)]
pub enum Error {
    /// Нет JPEG SOI (FF D8) — не mjpeg-кадр.
    NotJpeg,
    /// Размер буфера не сходится с кадром.
    BufferSize { got: usize, want: usize },
    /// Битый/оборванный JPEG (декодер отказал).
    Corrupt(String),
    /// YUYV→RGBA конверсия отказала.
    Convert(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotJpeg => write!(f, "MJPEG frame without SOI (FF D8)"),
            Error::BufferSize { got, want } => {
                write!(f, "decode buffer {got} pixels, expected {want}")
            }
            Error::Corrupt(e) => write!(f, "MJPEG decode error: {e}"),
            Error::Convert(e) => write!(f, "YUYV convert error: {e}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Decodes one raw camera frame into an egui image.
pub fn decode_frame(
    frame_format: PixelConfig,
    raw: &[u8],
    width: u32,
    height: u32,
) -> Result<egui::ColorImage> {
    let pixels = match frame_format {
        PixelConfig::Mjpeg => decode_pixels_mjpeg(raw, width, height)?,
        PixelConfig::Yuyv => decode_pixels_yuyv(raw, width, height)?,
    };
    Ok(egui::ColorImage {
        size: [width as usize, height as usize],
        source_size: egui::vec2(width as f32, height as f32),
        pixels,
    })
}

fn decode_pixels_yuyv(raw: &[u8], width: u32, height: u32) -> Result<Vec<egui::Color32>> {
    let mut pixels = bytemuck::allocation::zeroed_vec((width * height) as usize);
    let packed = yuv::YuvPackedImage {
        yuy: raw,
        yuy_stride: width * 2,
        width,
        height,
    };
    // UVC-камеры шлют BT.601 limited range (чёрный = Y16, белый = Y235);
    // Limited растягивает studio swing до полного RGB, иначе чёрный
    // приезжает в viewfinder как (16,16,16).
    yuv::yuyv422_to_rgba(
        &packed,
        bytemuck::cast_slice_mut(&mut pixels),
        width * 4,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
    )
    .map_err(|e| Error::Convert(format!("{e}")))?;
    Ok(pixels)
}

/// Декод MJPEG в готовый буфер (ровно width*height пикселей): без
/// аллокации и memset — для проигрывателя с постоянной памятью.
pub fn decode_pixels_mjpeg_into(
    raw: &[u8],
    width: u32,
    height: u32,
    pixels: &mut [egui::Color32],
) -> Result<()> {
    if pixels.len() != (width * height) as usize {
        return Err(Error::BufferSize {
            got: pixels.len(),
            want: (width * height) as usize,
        });
    }
    if !(raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xD8) {
        return Err(Error::NotJpeg);
    }
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
        zune_jpeg::zune_core::bytestream::ZCursor::new(raw),
        zune_jpeg::zune_core::options::DecoderOptions::default()
            .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA),
    );
    decoder
        .decode_into(bytemuck::cast_slice_mut(pixels))
        .map_err(|e| Error::Corrupt(format!("{e}")))?;
    Ok(())
}

fn decode_pixels_mjpeg(raw: &[u8], width: u32, height: u32) -> Result<Vec<egui::Color32>> {
    let mut pixels = bytemuck::allocation::zeroed_vec((width * height) as usize);
    decode_pixels_mjpeg_into(raw, width, height, &mut pixels)?;
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::decode_frame;
    use crate::config::camera::PixelConfig;

    /// YUYV limited: чёрный = Y16, белый = Y235 (U=V=128).
    #[test]
    fn yuyv_limited_range_black_is_zero() {
        let mut raw = Vec::new();
        for _ in 0..(2 * 2) / 2 {
            raw.extend_from_slice(&[16, 128, 16, 128]); // Y0 U Y1 V
        }
        let img = decode_frame(PixelConfig::Yuyv, &raw, 2, 2).unwrap();
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(0, 0, 0));
    }

    #[test]
    fn yuyv_limited_range_white_is_255() {
        let mut raw = Vec::new();
        for _ in 0..(2 * 2) / 2 {
            raw.extend_from_slice(&[235, 128, 235, 128]);
        }
        let img = decode_frame(PixelConfig::Yuyv, &raw, 2, 2).unwrap();
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(255, 255, 255));
    }
}
