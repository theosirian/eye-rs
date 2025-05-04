#![feature(try_blocks)]
#![feature(iter_chain)]
#![feature(type_alias_impl_trait)]
#![feature(iter_array_chunks)]

use std::{
    sync::{mpsc, Arc},
    thread,
    time::Instant,
};

use crossbeam::channel::{bounded, unbounded, Receiver, Sender};
use eye::colorconvert::Device;
use eye_hal::{
    format::PixelFormat,
    stream::Descriptor,
    traits::{Context as _, Device as _, Stream},
    Error, ErrorKind, PlatformContext,
};
use lazy_static::lazy_static;
use notan::{
    draw::{CreateDraw, DrawConfig, DrawImages},
    egui::{self, *},
    math::{self},
    prelude::*,
};
use object_pool::Pool;
use tokio::sync::RwLock;

lazy_static! {
    pub static ref POOL: Arc<Pool<Vec<u8>>> = Arc::new(Pool::new(128, || vec![0; 1280 * 720 * 4]));
}

#[derive(AppState)]
pub struct State {
    pub scene_rect: Rect,
    pub scene_sized: Option<SizedTexture>,

    pub frame_texture: Texture,
    pub frame_render_texture: RenderTexture,
    pub frames_tx: Option<Sender<Vec<u8>>>,
    pub frames_rx: Option<Receiver<Vec<u8>>>,

    pub width: Arc<RwLock<u32>>,
    pub height: Arc<RwLock<u32>>,
}

pub fn build_textures(
    gfx: &mut Graphics,
    width: u32,
    height: u32,
) -> (Texture, RenderTexture, Option<SizedTexture>) {
    match try {
        let frame_texture = gfx
            .create_texture()
            .with_format(TextureFormat::Rgb24)
            .with_filter(
                notan::app::TextureFilter::Nearest,
                notan::app::TextureFilter::Nearest,
            )
            .with_size(width, height)
            .from_empty_buffer(width, height)
            .build()?;
        let frame_render_texture = gfx
            .create_render_texture(width, height)
            .with_format(TextureFormat::Rgb24)
            .with_filter(
                notan::app::TextureFilter::Nearest,
                notan::app::TextureFilter::Nearest,
            )
            .build()?;
        let scene_sized = Some(gfx.egui_register_texture(&frame_render_texture));

        (frame_texture, frame_render_texture, scene_sized)
    } {
        Ok((frame_texture, frame_render_texture, scene_sized)) => {
            (frame_texture, frame_render_texture, scene_sized)
        }
        Err::<_, Box<dyn std::error::Error>>(e) => {
            log::error!("Failed to create textures: {e}");
            panic!("Failed to create textures: {}", e);
        }
    }
}

impl State {
    fn setup(gfx: &mut Graphics) -> Self {
        match try {
            let ctx = if let Some(ctx) = PlatformContext::all().next() {
                ctx
            } else {
                Err("No platform context available")?
            };

            // Create a list of valid capture devices in the system.
            let dev_descrs = ctx.devices()?;

            let args: Vec<String> = std::env::args().collect();
            let index = args[1].parse::<usize>().unwrap();

            // Print the supported formats for each device.
            let dev = ctx.open_device(&dev_descrs[index].uri)?;
            let dev = Device::new(dev)?;
            let mut stream_descr = dev
                .streams()?
                .into_iter()
                .filter(|desc| match desc {
                    Descriptor {
                        width,
                        height,
                        pixfmt: PixelFormat::Rgb(bits),
                        ..
                    } => *width == 1280 && *height == 720 && *bits == 24,
                    _ => false,
                })
                .collect::<Vec<_>>();

            stream_descr.sort_by(|a, b| a.interval.cmp(&b.interval));

            let stream_descr = stream_descr.first().unwrap();

            if stream_descr.pixfmt != PixelFormat::Rgb(24) {
                Err("No RGB3 streams available")?
            }

            println!("Selected stream:\n{:?}", stream_descr);

            let Descriptor { width, height, .. } = stream_descr;

            let (frames_tx, frames_rx) = bounded(1);

            std::thread::spawn({
                let stream_descr = stream_descr.to_owned();
                let frames_tx = frames_tx.clone();
                move || {
                    let tid = gettid::gettid();

                    log::info!("Starting camera");
                    let mut stream = match dev.start_stream(&stream_descr) {
                        Ok(stream) => stream,
                        Err(e) => {
                            log::error!("[{tid}] Failed to start camera: {e}");
                            return;
                        }
                    };

                    log::info!("[{tid}] Camera format is set to: {:?}", stream_descr);
                    log::info!("[{tid}] Camera Loop Started");
                    let mut count = 0usize;
                    loop {
                        count += 1;

                        if let Err::<_, Box<dyn std::error::Error>>(e) = try {
                            let frame = stream.next().ok_or("No frame available")??;

                            let (_, mut pooled) = POOL
                                .try_pull_owned()
                                .ok_or(format!("[{count}] 1-Skipping frame because POOL is dry!"))?
                                .detach();

                            pooled[0..frame.len()].copy_from_slice(&frame[..]);

                            let _ = frames_tx.send(pooled);
                        } {
                            log::error!("[{count}] Failed to decode camera frame: {e}");
                        }
                    }

                    // log::info!("[{tid}] Camera thread exited");
                }
            });

            let frames_tx = Some(frames_tx.clone());
            let frames_rx = Some(frames_rx.clone());

            let (frame_texture, frame_render_texture, scene_sized) =
                build_textures(gfx, *width, *height);

            let scene_sized = scene_sized;
            let frame_texture = frame_texture;
            let frame_render_texture = frame_render_texture;

            let scene_rect =
                Rect::from_min_size(pos2(0.0, 0.0), vec2(*width as f32, *height as f32));

            let width = Arc::new(RwLock::new(*width));
            let height = Arc::new(RwLock::new(*height));

            Self {
                scene_rect,
                scene_sized,

                frame_texture,
                frame_render_texture,
                frames_tx,
                frames_rx,

                width,
                height,
            }
        } {
            Ok(s) => s,
            Err::<_, Box<dyn std::error::Error>>(e) => {
                log::error!("Failed to create textures: {e}");
                panic!("Failed to create textures: {}", e);
            }
        }
    }
}

#[notan_main]
fn main() -> Result<(), String> {
    pretty_env_logger::init();

    let win = WindowConfig::new()
        .set_title("notan_eye-rs")
        .set_size(1366, 768)
        .set_resizable(false)
        .set_vsync(true)
        .set_lazy_loop(false)
        .set_multisampling(2)
        .set_high_dpi(true);

    notan::init_with(State::setup)
        .add_config(win)
        .add_config(EguiConfig)
        .add_config(DrawConfig)
        .update(update)
        .draw(draw)
        .build()
}

fn update(app: &mut App, assets: &mut Assets, _plugins: &mut Plugins, state: &mut State) {}

fn draw(app: &mut App, gfx: &mut Graphics, plugins: &mut Plugins, state: &mut State) {
    let mut output = plugins.egui(|ctx| {
        egui::TopBottomPanel::top("top_panel")
            .resizable(false)
            .exact_height(32.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("notan_eye-rs");
                    ui.with_layout(Layout::right_to_left(Align::LEFT), |ui| {
                        if ui.button("Quit").clicked() {
                            app.exit();
                        }
                        ui.add_sized(ui.available_size(), egui::Separator::default().horizontal());
                    });
                });
                ui.separator();
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let scene_sized = state.scene_sized;
            let scene = Scene::new().zoom_range(0.5..=8.0);

            let mut inner_rect = Rect::NAN;
            let response = scene
                .show(ui, &mut state.scene_rect, |ui| {
                    if let Some(scene_sized) = scene_sized {
                        ui.image(scene_sized);
                    }
                    inner_rect = ui.min_rect();
                })
                .response;

            if response.double_clicked() {
                state.scene_rect = inner_rect;
            }
        });
    });
    output.clear_color(Color::BLACK);

    if let Some(frames) = &state.frames_rx {
        if let Ok(buffer) = frames.try_recv() {
            if let Err::<_, Box<dyn std::error::Error>>(e) = try {
                gfx.update_texture(&mut state.frame_texture)
                    .with_data(buffer.as_slice())
                    .update()?;

                let width = *futures::executor::block_on(state.width.read()) as f32;
                let height = *futures::executor::block_on(state.height.read()) as f32;

                let mut draw = gfx.create_draw();
                draw.set_size(width, height);

                let transform = draw.transform();
                transform.push(math::Mat3::from_scale(math::Vec2::new(1.0, -1.0)));
                transform.push(math::Mat3::from_translation(math::Vec2::new(0.0, -height)));

                draw.image(&state.frame_texture)
                    .position(0.0, 0.0)
                    .size(width, height);

                gfx.render_to(&state.frame_render_texture, &draw);

                POOL.attach(buffer);
            } {
                log::error!("Failed to create texture from camera frame: {e}");
            }
        }
    }

    gfx.render(&output);
}
