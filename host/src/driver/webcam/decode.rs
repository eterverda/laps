pub enum Error {
    /// Skip this frame and keep capturing.
    Recoverable(String),
    /// Stop the capture thread.
    Unrecoverable(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Decodes one raw camera frame into an egui image.
pub fn decode_frame(
    frame_format: nokhwa::utils::FrameFormat,
    raw: &[u8],
    width: u32,
    height: u32,
) -> Result<egui::ColorImage> {
    let pixels = match frame_format {
        nokhwa::utils::FrameFormat::MJPEG => decode_pixels_mjpeg(raw, width, height)?,
        nokhwa::utils::FrameFormat::YUYV => decode_pixeld_yuyv(raw, width, height)?,
        other => {
            return Err(Error::Unrecoverable(format!(
                "unsupported frame format: {other:?}"
            )));
        }
    };
    Ok(egui::ColorImage {
        size: [width as usize, height as usize],
        source_size: egui::vec2(width as f32, height as f32),
        pixels,
    })
}

fn decode_pixeld_yuyv(raw: &[u8], width: u32, height: u32) -> Result<Vec<egui::Color32>> {
    let mut pixels = bytemuck::allocation::zeroed_vec((width * height) as usize);
    let packed = yuv::YuvPackedImage {
        yuy: raw,
        yuy_stride: width * 2,
        width,
        height,
    };
    yuv::yuyv422_to_rgba(
        &packed,
        bytemuck::cast_slice_mut(&mut pixels),
        width * 4,
        yuv::YuvRange::Full,
        yuv::YuvStandardMatrix::Bt601,
    )
    .map_err(|e| Error::Recoverable(format!("YUYV convert error: {e}")))?;
    Ok(pixels)
}

fn decode_pixels_mjpeg(raw: &[u8], width: u32, height: u32) -> Result<Vec<egui::Color32>> {
    if !(raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xD8) {
        return Err(Error::Recoverable(
            "MJPEG frame without JPEG SOI (FF D8) marker; \
                     the negotiated format may not be real MJPEG"
                .to_string(),
        ));
    }
    let mut pixels = bytemuck::allocation::zeroed_vec((width * height) as usize);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
        zune_jpeg::zune_core::bytestream::ZCursor::new(raw),
        zune_jpeg::zune_core::options::DecoderOptions::default()
            .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA),
    );
    decoder
        .decode_into(bytemuck::cast_slice_mut(&mut pixels))
        .map_err(|e| Error::Recoverable(format!("MJPEG decode error: {e}")))?;
    Ok(pixels)
}
