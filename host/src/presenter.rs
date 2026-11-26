//! 64-bit wgpu DX12 presenter.
//!
//! The host deliberately converts the 32-bit game's raw primary surface to
//! RGBA8 on the CPU. On Wine/D3DMetal the integer-texture + LUT path used by the
//! legacy in-process renderer has proven fragile; an RGBA8 texture with a plain
//! `textureLoad` is the safest presentation path for the 64-bit host.
use std::num::NonZeroIsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use raw_window_handle::{Win32WindowHandle, WindowsDisplayHandle};
use windows::Win32::Foundation::{HWND, HINSTANCE};

use ddraw_protocol as protocol;
use protocol::{FrameFormat, Header};

use crate::logging;

fn align_up(value: u32, alignment: u32) -> u32 {
    (value + alignment - 1) & !(alignment - 1)
}

fn debug_clear() -> Option<wgpu::Color> {
    let value = std::env::var("CNC_DDRAW_HOST_CLEAR").ok()?;
    let value = value.trim().to_ascii_lowercase();
    let (r, g, b) = match value.as_str() {
        "red" => (1.0, 0.0, 0.0),
        "green" => (0.0, 1.0, 0.0),
        "blue" => (0.0, 0.0, 1.0),
        "white" => (1.0, 1.0, 1.0),
        "magenta" => (1.0, 0.0, 1.0),
        "cyan" => (0.0, 1.0, 1.0),
        "yellow" => (1.0, 1.0, 0.0),
        other => {
            let hex = other.strip_prefix("0x").unwrap_or(other);
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            (
                ((rgb >> 16) & 0xff) as f32 / 255.0,
                ((rgb >> 8) & 0xff) as f32 / 255.0,
                (rgb & 0xff) as f32 / 255.0,
            )
        }
    };
    Some(wgpu::Color {
        r: r as f64,
        g: g as f64,
        b: b as f64,
        a: 1.0,
    })
}

struct UploadedFrame {
    raw_texture: wgpu::Texture,
    bindings: wgpu::BindGroup,
}

impl UploadedFrame {
    fn new(
        device: &wgpu::Device,
        pipeline: &wgpu::RenderPipeline,
        target_format: wgpu::TextureFormat,
        size: wgpu::Extent3d,
    ) -> Self {
        // Deliberately UNORM: D3DMetal's sRGB texture path is one of the
        // suspects for the black output. This may look slightly bright on an
        // sRGB swapchain, but it is visible and lets us separate colorspace
        // from a completely broken sample/upload path.
        let _ = target_format;
        let raw_format = wgpu::TextureFormat::Rgba8Unorm;
        let raw_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DirectDraw RGBA primary"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: raw_format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let raw_view = raw_texture.create_view(&Default::default());
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("DirectDraw frame bindings"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&raw_view),
            }],
        });
        Self {
            raw_texture,
            bindings,
        }
    }
}

fn instance_descriptor(backend: &str) -> wgpu::InstanceDescriptor {
    if std::env::var_os("WGPU_BACKEND").is_some() {
        return wgpu::InstanceDescriptor::from_env_or_default();
    }
    let backends = match backend.to_ascii_lowercase().as_str() {
        "vulkan" | "vk" => wgpu::Backends::VULKAN,
        _ => wgpu::Backends::DX12,
    };
    wgpu::InstanceDescriptor {
        backends,
        ..Default::default()
    }
}

pub struct Presenter {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    configured: bool,
    pipeline: wgpu::RenderPipeline,
    frame: Option<UploadedFrame>,
    staging: Vec<u8>,
    maintas: bool,
    clear_color: wgpu::Color,
    clear_only: bool,
    solid: bool,
    logged_frame: bool,
    logged_present: bool,
    frame_count: u64,
}

impl Presenter {
    pub fn new(
        hwnd: HWND,
        hinstance: HINSTANCE,
        backend: &str,
        maintas: bool,
    ) -> Result<Self, String> {
        let mut raw = Win32WindowHandle::new(
            NonZeroIsize::new(hwnd.0 as isize).ok_or("invalid host window")?,
        );
        raw.hinstance = Some(
            NonZeroIsize::new(hinstance.0 as isize).ok_or("invalid host instance")?,
        );

        let instance = wgpu::Instance::new(&instance_descriptor(backend));
        // SAFETY: `Overlay` outlives `Presenter` (declaration order in `run`), so
        // the raw HWND stays valid for the whole surface lifetime.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_window_handle: raw.into(),
                raw_display_handle: WindowsDisplayHandle::new().into(),
            })
        }
        .map_err(|err| format!("creating host surface: {err}"))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|err| format!("selecting DX12/D3DMetal adapter: {err}"))?;
        let info = adapter.get_info();
        logging::log_msg(&format!(
            "adapter: {:?} backend={:?} device_type={:?} driver={} {}",
            info.name, info.backend, info.device_type, info.driver, info.driver_info
        ));

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("DirectDraw host device"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|err| format!("creating host GPU device: {err}"))?;

        device.on_uncaptured_error(Arc::new(move |err: wgpu::Error| {
            logging::log_msg(&format!("wgpu uncaptured error: {err}"));
        }));
        device.set_device_lost_callback(|reason, message| {
            logging::log_msg(&format!("wgpu device lost ({reason:?}): {message}"));
        });

        let mut surface_cfg = surface
            .get_default_config(&adapter, 1, 1)
            .ok_or("no presentation format for the host overlay")?;
        // Wine DXGI has incomplete Mailbox/Immediate support.
        surface_cfg.present_mode = wgpu::PresentMode::Fifo;
        // Never let Wine/D3DMetal composite the desktop through the overlay.
        surface_cfg.alpha_mode = wgpu::CompositeAlphaMode::Opaque;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("DirectDraw RGBA present"),
            source: wgpu::ShaderSource::Wgsl(include_str!("present.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("DirectDraw presentation"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(surface_cfg.format.into())],
            }),
            multiview: None,
            cache: None,
        });

        let clear_color = debug_clear().unwrap_or(wgpu::Color::BLACK);
        let clear_only = std::env::var_os("CNC_DDRAW_HOST_CLEAR").is_some();
        let solid = std::env::var_os("CNC_DDRAW_HOST_SOLID").is_some();
        logging::log_msg(&format!(
            "presenter init: format={:?} clear_only={clear_only} solid={solid}",
            surface_cfg.format
        ));

        Ok(Self {
            surface,
            device,
            queue,
            config: surface_cfg,
            configured: false,
            pipeline,
            frame: None,
            staging: Vec::new(),
            maintas,
            clear_color,
            clear_only,
            solid,
            logged_frame: false,
            logged_present: false,
            frame_count: 0,
        })
    }

    pub fn upload(&mut self, header: &Header, base: *const u8) -> Result<(), String> {
        let format = FrameFormat::from_u32(header.format).ok_or("unknown frame format")?;
        let width = header.width;
        let height = header.height;
        let pitch = header.pitch;
        if width == 0
            || height == 0
            || width > protocol::MAX_WIDTH
            || height > protocol::MAX_HEIGHT
        {
            return Err(format!("unsupported frame size {width}x{height}"));
        }
        let bpp = format.bytes_per_pixel();
        let min_pitch = width
            .checked_mul(bpp)
            .ok_or_else(|| "frame pitch overflow".to_string())?;
        if pitch < min_pitch || u64::from(pitch) * u64::from(height) > u64::from(header.slot_bytes)
        {
            return Err("invalid frame pitch/length".to_string());
        }

        self.frame_count = self.frame_count.wrapping_add(1);
        if !self.logged_frame {
            self.logged_frame = true;
            let total = pitch as usize * height as usize;
            // SAFETY: The producer guarantees pitch * height bytes at `base`.
            let sample = unsafe { std::slice::from_raw_parts(base, total) };
            let nonzero = sample.iter().filter(|&&b| b != 0).count();
            let head = &sample[..sample.len().min(32)];
            let mid =
                &sample[(height as usize / 2) * pitch as usize..][..(pitch as usize).min(32)];
            logging::log_msg(&format!(
                "first frame: {width}x{height} pitch={pitch} format={format:?} lut_gen={} nonzero={nonzero}/{total} head={head:02x?} mid={mid:02x?} lut0={:?}",
                header.lut_gen,
                &header.lut[..4]
            ));
        } else if self.frame_count <= 5 || self.frame_count % 120 == 0 {
            let total = pitch as usize * height as usize;
            // SAFETY: The producer guarantees pitch * height bytes at `base`.
            let sample = unsafe { std::slice::from_raw_parts(base, total) };
            let nonzero = sample.iter().filter(|&&b| b != 0).count();
            let mid =
                &sample[(height as usize / 2) * pitch as usize..][..(pitch as usize).min(32)];
            logging::log_msg(&format!(
                "frame #{}: seq={} {width}x{height} format={format:?} nonzero={nonzero}/{total} mid={mid:02x?}",
                self.frame_count,
                header.seq.load(Ordering::Acquire),
            ));
        }

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        if self
            .frame
            .as_ref()
            .is_some_and(|frame| frame.raw_texture.size() != size)
        {
            self.frame = None;
        }
        let frame = self.frame.get_or_insert_with(|| {
            UploadedFrame::new(
                &self.device,
                &self.pipeline,
                self.config.format,
                size,
            )
        });

        let out_pitch = align_up(
            width.checked_mul(4).ok_or_else(|| "rgba pitch overflow".to_string())?,
            wgpu::COPY_BYTES_PER_ROW_ALIGNMENT,
        ) as usize;
        let needed = out_pitch
            .checked_mul(height as usize)
            .ok_or_else(|| "rgba staging size overflow".to_string())?;
        if self.staging.len() < needed {
            self.staging.resize(needed, 0);
        }
        // SAFETY: The producer guarantees pitch * height bytes at `base`.
        let source = unsafe { std::slice::from_raw_parts(base, pitch as usize * height as usize) };
        let lut = &header.lut;

        for y in 0..height as usize {
            let src = &source[y * pitch as usize..];
            let dst = &mut self.staging[y * out_pitch..(y + 1) * out_pitch];
            if self.solid {
                for x in 0..width as usize {
                    dst[x * 4..x * 4 + 4].copy_from_slice(&[255, 0, 0, 255]);
                }
            } else {
                match format {
                    FrameFormat::Indexed => {
                        for x in 0..width as usize {
                            let entry = lut[src[x] as usize];
                            dst[x * 4..x * 4 + 4].copy_from_slice(&entry);
                        }
                    }
                    FrameFormat::Rgb555 | FrameFormat::Rgb565 => {
                        for x in 0..width as usize {
                            let raw = u16::from_le_bytes([src[x * 2], src[x * 2 + 1]]);
                            let (r, g, b) = if format == FrameFormat::Rgb555 {
                                (
                                    (((raw >> 10) & 31) * 255 / 31) as u8,
                                    (((raw >> 5) & 31) * 255 / 31) as u8,
                                    ((raw & 31) * 255 / 31) as u8,
                                )
                            } else {
                                (
                                    (((raw >> 11) & 31) * 255 / 31) as u8,
                                    (((raw >> 5) & 63) * 255 / 63) as u8,
                                    ((raw & 31) * 255 / 31) as u8,
                                )
                            };
                            dst[x * 4] = lut[r as usize][0];
                            dst[x * 4 + 1] = lut[g as usize][1];
                            dst[x * 4 + 2] = lut[b as usize][2];
                            dst[x * 4 + 3] = 255;
                        }
                    }
                    FrameFormat::Xrgb => {
                        for x in 0..width as usize {
                            let raw = u32::from_le_bytes([
                                src[x * 4],
                                src[x * 4 + 1],
                                src[x * 4 + 2],
                                src[x * 4 + 3],
                            ]);
                            let r = ((raw >> 16) & 0xff) as u8;
                            let g = ((raw >> 8) & 0xff) as u8;
                            let b = (raw & 0xff) as u8;
                            dst[x * 4] = lut[r as usize][0];
                            dst[x * 4 + 1] = lut[g as usize][1];
                            dst[x * 4 + 2] = lut[b as usize][2];
                            dst[x * 4 + 3] = 255;
                        }
                    }
                }
            }
            dst[width as usize * 4..].fill(0);
        }

        self.queue.write_texture(
            frame.raw_texture.as_image_copy(),
            &self.staging[..needed],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(out_pitch as u32),
                rows_per_image: None,
            },
            size,
        );
        Ok(())
    }

    pub fn present(&mut self, target_w: u32, target_h: u32) -> Result<(), String> {
        if target_w == 0 || target_h == 0 {
            return Ok(());
        }
        if !self.configured || self.config.width != target_w || self.config.height != target_h {
            self.config.width = target_w;
            self.config.height = target_h;
            self.surface.configure(&self.device, &self.config);
            self.configured = true;
        }

        let mut acquired = self.surface.get_current_texture();
        if matches!(
            acquired,
            Err(wgpu::SurfaceError::Lost
                | wgpu::SurfaceError::Outdated
                | wgpu::SurfaceError::Other)
        ) {
            self.configured = false;
            self.surface.configure(&self.device, &self.config);
            self.configured = true;
            acquired = self.surface.get_current_texture();
        }
        let output = match acquired {
            Ok(output) => output,
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(err) => return Err(format!("acquiring host swapchain texture: {err}")),
        };
        self.configured = !output.suboptimal;

        let frame = self.frame.as_ref();
        if !self.clear_only && frame.is_none() {
            return Ok(());
        }
        if !self.logged_present {
            self.logged_present = true;
            logging::log_msg(&format!(
                "first present: target={target_w}x{target_h} frame={:?}",
                frame.map(|f| f.raw_texture.size())
            ));
        }
        let view = output.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("DirectDraw present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let Some(frame) = frame {
                let frame_size = frame.raw_texture.size();
                let (vx, vy, vw, vh) = compute_viewport(
                    frame_size.width,
                    frame_size.height,
                    target_w,
                    target_h,
                    self.maintas,
                );
                pass.set_viewport(vx as f32, vy as f32, vw as f32, vh as f32, 0.0, 1.0);
                pass.set_scissor_rect(vx, vy, vw, vh);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &frame.bindings, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        output.present();
        Ok(())
    }
}

fn compute_viewport(
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
    maintas: bool,
) -> (u32, u32, u32, u32) {
    if !maintas || width == 0 || height == 0 || target_w == 0 || target_h == 0 {
        return (0, 0, target_w.max(1), target_h.max(1));
    }
    let w = (target_h as u64 * width as u64 / height as u64).clamp(1, target_w as u64) as u32;
    let h = (target_w as u64 * height as u64 / width as u64).clamp(1, target_h as u64) as u32;
    ((target_w - w) / 2, (target_h - h) / 2, w, h)
}
