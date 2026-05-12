use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{RequestedFormat, RequestedFormatType};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use yuv::{YuvPackedImage, YuvRange, YuvStandardMatrix};

pub struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

type FrameSlot = Arc<Mutex<Option<Frame>>>;

type FrameCallback = Box<dyn Fn() + Send + Sync>;

pub struct Webcam {
    slot: FrameSlot,
    texture: Option<egui::TextureHandle>,
    running: Arc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl Webcam {
    pub fn start(on_frame: FrameCallback) -> Option<Self> {
        let cameras = match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
            Ok(c) => c,
            Err(e) => {
                log::error!("failed to query cameras: {}", e);
                return None;
            }
        };

        let info = cameras.first()?;
        let index = info.index().clone();
        let name = info.human_name();
        log::info!("starting capture from [{}] {}", index, name);

        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let slot_clone = Arc::clone(&slot);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let thread = thread::spawn(move || {
            let format =
                RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
            let mut camera = match Camera::new(index, format) {
                Ok(cam) => cam,
                Err(e) => {
                    log::error!("failed to open camera: {}", e);
                    return;
                }
            };

            if let Err(e) = camera.open_stream() {
                log::error!("failed to open stream: {}", e);
                return;
            }

            let fmt = camera.camera_format();
            let width = fmt.resolution().width();
            let height = fmt.resolution().height();
            log::info!("capture stream opened, format: {:?}", fmt);

            let mut rgba_buf = vec![0u8; (width * height * 4) as usize];

            loop {
                if !running_clone.load(Ordering::Relaxed) {
                    log::info!("capture thread stopping");
                    break;
                }

                let raw = match camera.frame_raw() {
                    Ok(r) => r,
                    Err(e) => {
                        log::error!("frame capture error: {}", e);
                        thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                };

                let packed = YuvPackedImage {
                    yuy: &raw,
                    yuy_stride: width * 2,
                    width,
                    height,
                };
                yuv::yuyv422_to_rgba(
                    &packed,
                    &mut rgba_buf,
                    width * 4,
                    YuvRange::Full,
                    YuvStandardMatrix::Bt601,
                )
                .unwrap();

                let frame = Frame {
                    width,
                    height,
                    rgba: rgba_buf.clone(),
                };

                *slot_clone.lock().unwrap() = Some(frame);

                on_frame();
            }

            if let Err(e) = camera.stop_stream() {
                log::error!("failed to stop stream: {}", e);
            }
        });

        Some(Self {
            slot,
            texture: None,
            running,
            _thread: thread,
        })
    }

    pub fn update(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        let frame = {
            let mut guard = self.slot.lock().unwrap();
            guard.take()
        };

        if let Some(frame) = frame {
            let size = [frame.width as usize, frame.height as usize];
            let pixels: Vec<egui::Color32> = bytemuck::cast_vec(frame.rgba);
            let color_image = egui::ColorImage { size, pixels };

            match &mut self.texture {
                Some(tex) => {
                    tex.set(color_image, egui::TextureOptions::NEAREST);
                }
                None => {
                    self.texture = Some(ctx.load_texture(
                        "camera",
                        color_image,
                        egui::TextureOptions::NEAREST,
                    ));
                }
            }
        }

        self.texture.as_ref()
    }
}

impl Drop for Webcam {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}
